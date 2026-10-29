use crate::{Bounds, Pixels, Size};

/// Measure only actual children, independently of their inherited scroll origin.
pub(super) fn children_size(children: impl Iterator<Item = Bounds<Pixels>>) -> Option<Size<Pixels>> {
    children.reduce(|bounds, child| bounds.union(&child)).map(|bounds| bounds.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{point, px, size};

    #[test]
    fn a_child_above_the_window_keeps_its_real_height() {
        let child = Bounds::new(point(px(240.0), px(-500.0)), size(px(180.0), px(140.0)));
        assert_eq!(children_size([child].into_iter()), Some(size(px(180.0), px(140.0))));
    }

    #[test]
    fn a_child_left_of_the_window_keeps_its_real_width() {
        let child = Bounds::new(point(px(-500.0), px(120.0)), size(px(180.0), px(140.0)));
        assert_eq!(children_size([child].into_iter()), Some(size(px(180.0), px(140.0))));
    }

    #[test]
    fn translating_multiple_children_does_not_change_the_union_size() {
        let children = [
            Bounds::new(point(px(120.0), px(140.0)), size(px(100.0), px(50.0))),
            Bounds::new(point(px(210.0), px(190.0)), size(px(80.0), px(40.0))),
        ];
        let expected = Some(size(px(170.0), px(90.0)));
        assert_eq!(children_size(children.into_iter()), expected);
        let translated = children.into_iter().map(|mut bounds| {
            bounds.origin += point(px(-900.0), px(-1300.0));
            bounds
        });
        assert_eq!(children_size(translated), expected);
    }

    #[test]
    fn an_empty_anchor_has_no_child_size() {
        assert_eq!(children_size(std::iter::empty()), None);
    }
}
