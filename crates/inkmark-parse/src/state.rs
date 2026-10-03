use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use inkmark_buffer::{Bias, Change, Document};

use crate::map::SourceMap;
use crate::{DefinitionLabel, MarkdownParser, ParseOutput, normalize_label};

/// Quiet time after the last edit before a full background parse starts.
pub const DEBOUNCE: Duration = Duration::from_millis(24);
/// Regions larger than this skip the local reparse and wait for the
/// background parse (e.g. edits inside a huge top-level list).
const LOCAL_REPARSE_LIMIT: usize = 64 * 1024;
/// Catching up through more edits than this is left to the background parse.
const MAX_CATCH_UP_EDITS: usize = 64;

impl ParseOutput {
    /// A placeholder for a document not parsed yet.
    pub fn unparsed(len: usize) -> Self {
        Self {
            blocks: Default::default(),
            map: SourceMap::unparsed(len),
            link_defs: Default::default(),
            footnotes: Default::default(),
            definitions: Vec::new(),
        }
    }

    fn rebase(&mut self, change: &Change) {
        self.map.rebase(change);
        self.blocks.rebase(change);
        for d in &mut self.definitions {
            d.range = change.map(d.range.start, Bias::Left)..change.map(d.range.end, Bias::Right);
        }
    }

    /// Brings this output (a parse of `doc` at epoch `since`) up to date:
    /// shifts everything through the edits since, then re-parses the
    /// top-level blocks they touched. Returns `false` if the edit log no
    /// longer reaches back to `since`.
    pub fn catch_up(&mut self, parser: &dyn MarkdownParser, doc: &Document, since: u64) -> bool {
        let Some(changes) = doc.log().changes_since(since) else {
            return false;
        };
        let changes: Vec<Change> = changes.copied().collect();
        for change in &changes {
            self.rebase(change);
        }
        if changes.len() > MAX_CATCH_UP_EDITS {
            return true;
        }
        // Where each edit's new text ended up after the edits that followed.
        let mut dirty: Vec<Range<usize>> = changes
            .iter()
            .enumerate()
            .map(|(i, c)| {
                changes[i + 1..]
                    .iter()
                    .fold(c.start..c.new_end, |r, later| {
                        later.map(r.start, Bias::Left)..later.map(r.end, Bias::Right)
                    })
            })
            .collect();
        dirty.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for r in dirty {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        // Local reparses don't change lengths, so order doesn't matter.
        for r in merged {
            self.reparse_local(parser, doc, r);
        }
        true
    }

    /// Re-parses the top-level blocks around `dirty` (current offsets) in place.
    fn reparse_local(&mut self, parser: &dyn MarkdownParser, doc: &Document, dirty: Range<usize>) {
        let mut region = match self.blocks.top_level_span(dirty.clone(), 1) {
            Some(r) => r.start.min(dirty.start)..r.end.max(dirty.end),
            None => 0..doc.len(),
        };
        // Line-align, then widen to whole top-level blocks until stable.
        loop {
            let start = doc.line_to_byte(doc.byte_to_line(region.start));
            let end_line = doc.byte_to_line(region.end);
            let end = if region.end == doc.line_to_byte(end_line) {
                region.end
            } else {
                (doc.line_range(end_line).end + 1).min(doc.len())
            };
            let mut widened = start..end;
            if let Some(blocks) = self.blocks.top_level_span(widened.clone(), 0) {
                widened = widened.start.min(blocks.start)..widened.end.max(blocks.end);
            }
            if widened == region {
                break;
            }
            region = widened;
        }
        if region.is_empty() || region.len() > LOCAL_REPARSE_LIMIT {
            return;
        }
        let changed = self.splice_parsed(parser, doc, region.clone(), true);
        // The region above already has the new styles. References in other
        // blocks still wear the style from the last full parse.
        self.restyle_references(parser, doc, &region, &changed);
    }

    /// Parses `region` with stand-in definitions for labels defined outside
    /// it and splices the spans and blocks back. When `update_defs` is set
    /// (the edited region, not a later restyle), definitions inside `region`
    /// are replaced and labels whose resolved definition changed are returned.
    fn splice_parsed(
        &mut self,
        parser: &dyn MarkdownParser,
        doc: &Document,
        region: Range<usize>,
        update_defs: bool,
    ) -> Vec<DefinitionLabel> {
        let mut text = doc.slice(region.clone()).into_owned();
        let len = text.len();
        self.append_standins(&mut text, &region);
        let mut local = parser.parse(&text);
        let offset = region.start;
        let spans = local
            .map
            .iter()
            .filter(|s| s.range.start < len)
            .map(|mut s| {
                s.range = s.range.start + offset..s.range.end.min(len) + offset;
                s
            })
            .collect();
        let blocks = std::mem::take(&mut local.blocks)
            .iter()
            .filter(|b| b.range.start < len || (b.range.is_empty() && b.range.start == len))
            .map(|mut b| {
                b.range = b.range.start + offset..b.range.end.min(len) + offset;
                b
            })
            .collect();
        let changed = if update_defs {
            self.replace_definitions(&region, len, &mut local)
        } else {
            Vec::new()
        };
        self.map.splice(region.clone(), spans);
        self.blocks.splice(region, blocks);
        changed
    }

    /// Definitions that live outside `region`, written after its bytes so a
    /// reference inside it still parses as a link or footnote. A deleted
    /// definition collapses to an empty range and is not revived here.
    /// Stand-in spans start at `len` and are dropped; a placeholder
    /// destination is enough, because a reference's spans don't carry the URL.
    fn append_standins(&self, text: &mut String, region: &Range<usize>) {
        let mut emitted = false;
        let mut seen: Vec<&DefinitionLabel> = Vec::new();
        for def in &self.definitions {
            if def.range.is_empty() || !strictly_outside(&def.range, region) {
                continue;
            }
            if seen.contains(&&def.label) {
                continue;
            }
            let Some(line) = standin_line(&def.label) else {
                continue;
            };
            if !emitted {
                text.push_str("\n\n");
                emitted = true;
            }
            text.push_str(&line);
            seen.push(&def.label);
        }
    }

    /// Re-parses top-level blocks outside `region` that mention a label whose
    /// definition just changed, so their references gain or lose link and
    /// footnote style without waiting for the full parse. A block over the
    /// local-reparse limit waits, as an edit inside it would.
    fn restyle_references(
        &mut self,
        parser: &dyn MarkdownParser,
        doc: &Document,
        region: &Range<usize>,
        changed: &[DefinitionLabel],
    ) {
        if changed.is_empty() {
            return;
        }
        let blocks: Vec<Range<usize>> = self
            .blocks
            .top_level()
            .filter(|b| strictly_outside(&b.range, region))
            .map(|b| b.range)
            .filter(|r| !r.is_empty() && r.len() <= LOCAL_REPARSE_LIMIT)
            .filter(|r| mentions(&doc.slice(r.clone()), changed))
            .collect();
        for range in blocks {
            // The text of these blocks did not change, so their definitions
            // stay as `replace_definitions` just left them.
            let _ = self.splice_parsed(parser, doc, range, false);
        }
    }
}

fn strictly_outside(range: &Range<usize>, region: &Range<usize>) -> bool {
    range.end <= region.start || range.start >= region.end
}

fn standin_line(label: &DefinitionLabel) -> Option<String> {
    let (body, line) = match label {
        DefinitionLabel::Link(label) => (label.as_str(), format!("[{label}]: x\n")),
        DefinitionLabel::Footnote(label) => (label.as_str(), format!("[^{label}]: x\n")),
    };
    // A newline would escape the stand-in and could be read as part of the
    // region above it. Anything else is the label as stored.
    if body.is_empty() || body.contains(['\n', '\r']) {
        None
    } else {
        Some(line)
    }
}

/// Whether `text` holds a link or footnote reference to one of `changed`.
/// An unclosed `[` reparses the block: missing a real reference leaves the
/// old style up until the full parse.
fn mentions(text: &str, changed: &[DefinitionLabel]) -> bool {
    let mut rest = text;
    while let Some(rel) = rest.find('[') {
        let after = &rest[rel + 1..];
        let (footnote, body) = match after.strip_prefix('^') {
            Some(stripped) => (true, stripped),
            None => (false, after),
        };
        let Some(content) = bracket_body(body) else {
            return true;
        };
        let hit = if footnote {
            changed.iter().any(|label| match label {
                DefinitionLabel::Footnote(name) => {
                    name == content || normalize_label(name) == normalize_label(content)
                }
                DefinitionLabel::Link(_) => false,
            })
        } else {
            let norm = normalize_label(content);
            changed
                .iter()
                .any(|label| matches!(label, DefinitionLabel::Link(name) if name == &norm))
        };
        if hit {
            return true;
        }
        let consumed = rel + 1 + (after.len() - body.len()) + content.len() + 1;
        rest = &rest[consumed..];
    }
    false
}

/// The insides of a `[...]` group, or `None` when a nested `[` or the end of
/// the text makes the group ambiguous.
fn bracket_body(text: &str) -> Option<&str> {
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            ']' => return Some(&text[..i]),
            '[' => return None,
            _ => {}
        }
    }
    None
}

impl ParseOutput {
    /// After a local reparse of `region`: the definitions that were in it
    /// are replaced by the ones it has now (from `local`, a parse of its
    /// `len` bytes plus stand-ins after them). A label nothing defines any
    /// more stops resolving, instead of lingering until the full parse.
    /// Labels whose resolved destination changed are returned so references
    /// outside `region` can be restyled.
    fn replace_definitions(
        &mut self,
        region: &Range<usize>,
        len: usize,
        local: &mut ParseOutput,
    ) -> Vec<DefinitionLabel> {
        let inside = |r: &Range<usize>| region.start <= r.start && r.end <= region.end;
        let mut touched: Vec<DefinitionLabel> = Vec::new();
        self.definitions.retain(|d| {
            let keep = !inside(&d.range);
            if !keep {
                touched.push(d.label.clone());
            }
            keep
        });
        let added = std::mem::take(&mut local.definitions)
            .into_iter()
            .filter(|d| d.range.start < len)
            .map(|mut d| {
                d.range = d.range.start + region.start..d.range.end.min(len) + region.start;
                d
            });
        for d in added {
            touched.push(d.label.clone());
            self.definitions.push(d);
        }
        self.definitions.sort_by_key(|d| d.range.start);
        // Each label touched resolves to its earliest remaining definition,
        // wherever that is, or not at all. Unchanged labels are not returned:
        // an edit that merely widened over a definition must not reparse
        // every reference to it.
        let mut seen: Vec<DefinitionLabel> = Vec::new();
        let mut changed: Vec<DefinitionLabel> = Vec::new();
        for label in &touched {
            if seen.contains(label) {
                continue;
            }
            seen.push(label.clone());
            let previous = self.resolved(label);
            self.retarget(label);
            if self.resolved(label) != previous {
                changed.push(label.clone());
            }
        }
        changed
    }

    /// The destination a label resolves to, or `None` when it doesn't.
    /// Footnotes have no destination; presence is an empty string.
    fn resolved(&self, label: &DefinitionLabel) -> Option<String> {
        match label {
            DefinitionLabel::Link(label) => self.link_defs.get(label).cloned(),
            DefinitionLabel::Footnote(label) => {
                self.footnotes.contains(label).then_some(String::new())
            }
        }
    }

    fn retarget(&mut self, label: &DefinitionLabel) {
        let dest = self
            .definitions
            .iter()
            .find(|d| &d.label == label)
            .map(|d| d.dest.clone());
        match label {
            DefinitionLabel::Link(label) => match dest {
                Some(dest) => {
                    self.link_defs.insert(label.clone(), dest);
                }
                None => {
                    self.link_defs.remove(label);
                }
            },
            DefinitionLabel::Footnote(label) => {
                if dest.is_some() {
                    self.footnotes.insert(label.clone());
                } else {
                    self.footnotes.remove(label);
                }
            }
        }
    }
}

struct Job {
    generation: u64,
    epoch: u64,
    text: String,
}

/// Keeps a document's parse current: a local reparse on every edit for
/// immediate feedback, and a debounced full parse on a worker thread that
/// replaces it once it arrives.
pub struct ParseState {
    parser: Arc<dyn MarkdownParser>,
    output: ParseOutput,
    /// Document epoch `output` reflects.
    epoch: u64,
    /// Epoch of the last full parse folded into `output`.
    full_epoch: Option<u64>,
    requested: Option<u64>,
    last_change: Instant,
    /// Bumped by `reset`; jobs and results carry it so a parse of the
    /// previous document still in flight can't land on the new one.
    generation: u64,
    jobs: Sender<Job>,
    results: Receiver<(u64, u64, ParseOutput)>,
}

impl ParseState {
    /// Starts the worker. `on_result` runs on the worker thread whenever a
    /// parse finishes, e.g. to wake the UI.
    pub fn new(
        parser: Arc<dyn MarkdownParser>,
        doc: &Document,
        on_result: impl Fn() + Send + 'static,
    ) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (result_tx, results) = mpsc::channel();
        let worker_parser = Arc::clone(&parser);
        std::thread::Builder::new()
            .name("inkmark-parse".into())
            .spawn(move || {
                while let Ok(mut job) = job_rx.recv() {
                    // Only the newest text matters.
                    while let Ok(newer) = job_rx.try_recv() {
                        job = newer;
                    }
                    let output = worker_parser.parse(&job.text);
                    if result_tx.send((job.generation, job.epoch, output)).is_err() {
                        break;
                    }
                    on_result();
                }
            })
            .expect("spawn parse thread");
        let mut state = Self {
            parser,
            output: ParseOutput::default(),
            epoch: 0,
            full_epoch: None,
            requested: None,
            last_change: Instant::now(),
            generation: 0,
            jobs,
            results,
        };
        state.reset(doc);
        state
    }

    /// Starts over for a newly loaded document.
    pub fn reset(&mut self, doc: &Document) {
        self.output = ParseOutput::unparsed(doc.len());
        self.epoch = doc.epoch();
        self.full_epoch = None;
        self.requested = None;
        // No debounce for the first parse.
        self.last_change = Instant::now() - DEBOUNCE;
        // Results for the previous document, queued or still being
        // computed, carry the old generation and are ignored.
        self.generation += 1;
    }

    pub fn output(&self) -> &ParseOutput {
        &self.output
    }

    /// Whether `output` comes from a full parse of the current text (as
    /// opposed to local reparses that may miss non-local effects).
    pub fn is_settled(&self) -> bool {
        self.full_epoch == Some(self.epoch)
    }

    /// Call every frame. Returns how long until it wants to run again (a
    /// pending debounce), if anything is pending.
    pub fn update(&mut self, doc: &Document) -> Option<Duration> {
        let now = Instant::now();
        while let Ok((generation, epoch, output)) = self.results.try_recv() {
            if generation == self.generation {
                self.accept(doc, epoch, output);
            }
        }
        if doc.epoch() != self.epoch {
            if !self.output.catch_up(self.parser.as_ref(), doc, self.epoch) {
                self.output = ParseOutput::unparsed(doc.len());
            }
            self.epoch = doc.epoch();
            self.last_change = now;
        }
        if self.full_epoch == Some(self.epoch) || self.requested == Some(self.epoch) {
            return None;
        }
        let quiet = now - self.last_change;
        if quiet < DEBOUNCE {
            return Some(DEBOUNCE - quiet);
        }
        let _ = self.jobs.send(Job {
            generation: self.generation,
            epoch: self.epoch,
            text: String::from(doc.rope()),
        });
        self.requested = Some(self.epoch);
        None
    }

    fn accept(&mut self, doc: &Document, epoch: u64, mut output: ParseOutput) {
        if self.full_epoch.is_some_and(|e| e >= epoch) || epoch > doc.epoch() {
            return;
        }
        if epoch < doc.epoch() && !output.catch_up(self.parser.as_ref(), doc, epoch) {
            return;
        }
        self.output = output;
        self.epoch = doc.epoch();
        self.full_epoch = Some(epoch);
    }
}
