use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use inkmark_buffer::{Bias, Change, Document};

use crate::map::SourceMap;
use crate::{MarkdownParser, ParseOutput};

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
        let mut text = doc.slice(region.clone()).into_owned();
        let len = text.len();
        // A footnote reference only parses as one if its definition exists,
        // and that's usually elsewhere in the file. Stand-in definitions
        // after the region keep `[^1]` a footnote while its paragraph is
        // typed in; everything they produce is past `len` and dropped.
        if !self.footnotes.is_empty() {
            text.push_str("\n\n");
            for label in &self.footnotes {
                text.push_str(&format!("[^{label}]: x\n"));
            }
        }
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
        self.replace_definitions(&region, len, &mut local);
        self.map.splice(region.clone(), spans);
        self.blocks.splice(region, blocks);
    }
}

impl ParseOutput {
    /// After a local reparse of `region`: the definitions that were in it
    /// are replaced by the ones it has now (from `local`, a parse of its
    /// `len` bytes plus stand-ins after them). A label nothing defines any
    /// more stops resolving, instead of lingering until the full parse.
    fn replace_definitions(&mut self, region: &Range<usize>, len: usize, local: &mut ParseOutput) {
        let inside = |r: &Range<usize>| region.start <= r.start && r.end <= region.end;
        let mut touched: Vec<crate::DefinitionLabel> = Vec::new();
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
        // wherever that is, or not at all.
        for label in touched {
            let first = self.definitions.iter().find(|d| d.label == label);
            match (label, first) {
                (crate::DefinitionLabel::Link(l), Some(d)) => {
                    self.link_defs.insert(l, d.dest.clone());
                }
                (crate::DefinitionLabel::Link(l), None) => {
                    self.link_defs.remove(&l);
                }
                (crate::DefinitionLabel::Footnote(l), Some(_)) => {
                    self.footnotes.insert(l);
                }
                (crate::DefinitionLabel::Footnote(l), None) => {
                    self.footnotes.remove(&l);
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
