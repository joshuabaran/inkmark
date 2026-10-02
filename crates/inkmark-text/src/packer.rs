/// Shelf packer for one square atlas page.
///
/// Glyphs from the same font size have similar heights, so best-fit shelves
/// waste little space and allocation is a short linear scan.
pub(crate) struct ShelfPacker {
    size: u32,
    shelves: Vec<Shelf>,
    next_y: u32,
}

struct Shelf {
    y: u32,
    height: u32,
    next_x: u32,
}

impl ShelfPacker {
    pub(crate) fn new(size: u32) -> Self {
        Self {
            size,
            shelves: Vec::new(),
            next_y: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.shelves.clear();
        self.next_y = 0;
    }

    /// Returns the top-left corner of a free `w`×`h` region, or `None` if the page is full.
    pub(crate) fn alloc(&mut self, w: u32, h: u32) -> Option<[u32; 2]> {
        if w > self.size || h > self.size {
            return None;
        }
        // Best fit: the shortest shelf that holds `h` without wasting more than half of it.
        let best = self
            .shelves
            .iter_mut()
            .filter(|s| s.height >= h && s.height <= h * 2 && self.size - s.next_x >= w)
            .min_by_key(|s| s.height);
        if let Some(shelf) = best {
            let pos = [shelf.next_x, shelf.y];
            shelf.next_x += w;
            return Some(pos);
        }
        if self.size - self.next_y < h {
            return None;
        }
        let y = self.next_y;
        self.shelves.push(Shelf {
            y,
            height: h,
            next_x: w,
        });
        self.next_y += h;
        Some([0, y])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: ([u32; 2], [u32; 2]), b: ([u32; 2], [u32; 2])) -> bool {
        let ((ax, ay), (aw, ah)) = ((a.0[0], a.0[1]), (a.1[0], a.1[1]));
        let ((bx, by), (bw, bh)) = ((b.0[0], b.0[1]), (b.1[0], b.1[1]));
        ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
    }

    #[test]
    fn allocations_never_overlap_and_stay_in_bounds() {
        let mut packer = ShelfPacker::new(256);
        let mut placed = Vec::new();
        let mut seed = 0x2545_f491_u32;
        loop {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let size = [1 + seed % 24, 4 + (seed >> 8) % 20];
            let Some(pos) = packer.alloc(size[0], size[1]) else {
                break;
            };
            assert!(pos[0] + size[0] <= 256 && pos[1] + size[1] <= 256);
            for &other in &placed {
                assert!(!overlaps((pos, size), other));
            }
            placed.push((pos, size));
        }
        assert!(
            placed.len() > 100,
            "page filled too early: {}",
            placed.len()
        );
    }

    #[test]
    fn rejects_oversized_and_reuses_after_clear() {
        let mut packer = ShelfPacker::new(64);
        assert_eq!(packer.alloc(65, 1), None);
        assert_eq!(packer.alloc(64, 64), Some([0, 0]));
        assert_eq!(packer.alloc(1, 1), None);
        packer.clear();
        assert_eq!(packer.alloc(8, 8), Some([0, 0]));
        assert_eq!(packer.alloc(8, 8), Some([8, 0]));
    }
}
