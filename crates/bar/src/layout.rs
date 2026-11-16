//! Pure module placement. Side groups get their natural width, the center is
//! centered on the screen when possible and otherwise stays between the sides.
use crate::config::Module;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub module: Module,
    pub x: i32,
    pub width: i32,
}

pub fn arrange(groups: &[Vec<(Module, i32)>; 3], width: i32, padding: i32, gap: i32) -> Vec<Cell> {
    let left = padding.min(width / 2).max(0);
    let right = (width - left).max(left);
    let group_width = |items: &[(Module, i32)]| {
        items.iter().map(|(_, width)| width).sum::<i32>()
            + items.len().saturating_sub(1) as i32 * gap
    };
    let mut result = Vec::new();
    let mut emit = |items: &[(Module, i32)], start: i32, end: i32| {
        let mut x = start;
        for &(module, natural) in items {
            let width = natural.min(end - x).max(0);
            if width > 0 {
                result.push(Cell { module, x, width });
            }
            x += width + gap;
        }
    };
    let left_end = (left + group_width(&groups[0])).min(right);
    emit(&groups[0], left, left_end);
    let right_start = (right - group_width(&groups[2]))
        .max(left_end + if groups[0].is_empty() { 0 } else { gap })
        .min(right);
    emit(&groups[2], right_start, right);
    let min_center = (left_end + if groups[0].is_empty() { 0 } else { gap }).min(right_start);
    let max_center = (right_start - if groups[2].is_empty() { 0 } else { gap }).max(min_center);
    let center_width = group_width(&groups[1]).min(max_center - min_center).max(0);
    let center_start = ((width - center_width) / 2).clamp(min_center, max_center - center_width);
    emit(&groups[1], center_start, center_start + center_width);
    result
}

pub fn workspace_page(active: u32, count: u32, limit: u32) -> std::ops::Range<u32> {
    let visible = count.min(limit);
    let start = active
        .saturating_sub(visible / 2)
        .min(count.saturating_sub(visible));
    start..start + visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use Module::*;

    #[test]
    fn narrow_bars_clip_modules_without_overlap_or_negative_widths() {
        let groups = [
            vec![(Workspaces, 360), (Mode, 70)],
            vec![(Title, 900)],
            vec![(Clock, 80)],
        ];
        for width in [1, 20, 200, 600, 1920] {
            let mut cells = arrange(&groups, width, 8, 8);
            cells.sort_by_key(|cell| cell.x);
            for cell in &cells {
                assert!(cell.x >= 0 && cell.width > 0 && cell.x + cell.width <= width);
            }
            assert!(
                cells
                    .windows(2)
                    .all(|pair| pair[0].x + pair[0].width <= pair[1].x)
            );
        }
    }

    #[test]
    fn workspace_page_always_contains_the_active_workspace() {
        for active in 0..100 {
            let page = workspace_page(active, 100, 12);
            assert_eq!(page.len(), 12);
            assert!(page.contains(&active));
        }
    }
}
