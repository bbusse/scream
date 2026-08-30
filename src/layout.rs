// Window mode tiles every captured toplevel into an equal grid cell on the
// compositor canvas. The grid is as square as it can be: ceil(sqrt(n)) columns,
// then as many rows as that needs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub cols: u32,
    pub rows: u32,
    pub cell_w: u32,
    pub cell_h: u32,
}

impl Grid {
    pub fn new(n: usize, out_w: u32, out_h: u32) -> Grid {
        let cols = ((n as f64).sqrt().ceil() as u32).max(1);
        let rows = (n as u32).div_ceil(cols).max(1);

        Grid { cols, rows, cell_w: out_w / cols, cell_h: out_h / rows }
    }

    // Top-left corner of cell i, filling rows left to right.
    pub fn cell_origin(&self, i: usize) -> (u32, u32) {
        let col = i as u32 % self.cols;
        let row = i as u32 / self.cols;

        (col * self.cell_w, row * self.cell_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_window_fills_the_canvas() {
        assert_eq!(Grid::new(1, 1920, 1080),
                   Grid { cols: 1, rows: 1, cell_w: 1920, cell_h: 1080 });
    }

    #[test]
    fn two_windows_split_side_by_side() {
        let g = Grid::new(2, 1920, 1080);
        assert_eq!((g.cols, g.rows), (2, 1));
        assert_eq!(g.cell_w, 960);
    }

    #[test]
    fn three_windows_use_a_two_by_two_grid() {
        let g = Grid::new(3, 1000, 1000);
        assert_eq!((g.cols, g.rows), (2, 2));
        assert_eq!(g.cell_origin(0), (0, 0));
        assert_eq!(g.cell_origin(1), (500, 0));
        assert_eq!(g.cell_origin(2), (0, 500));
    }

    #[test]
    fn five_windows_spill_into_a_third_column() {
        let g = Grid::new(5, 1500, 1000);
        assert_eq!((g.cols, g.rows), (3, 2));
    }

    #[test]
    fn zero_windows_does_not_divide_by_zero() {
        assert_eq!(Grid::new(0, 1920, 1080).cols, 1);
    }
}
