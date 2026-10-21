use std::cmp::Ordering;

use druid::{
    widget::{prelude::*, ListIter},
    Point, Rect, WidgetPod,
};

/// Like `List`, but lays its children out left to right, starting a new row when
/// the next one doesn't fit the width: a grid that follows the window size.
pub struct Wrap<T> {
    closure: Box<dyn Fn() -> Box<dyn Widget<T>>>,
    children: Vec<WidgetPod<T, Box<dyn Widget<T>>>>,
    spacing: f64,
}

impl<T: Data> Wrap<T> {
    pub fn new<W: Widget<T> + 'static>(closure: impl Fn() -> W + 'static) -> Self {
        Self {
            closure: Box::new(move || Box::new(closure())),
            children: Vec::new(),
            spacing: 0.0,
        }
    }

    pub fn with_spacing(mut self, spacing: f64) -> Self {
        self.spacing = spacing;
        self
    }

    /// Creates or removes children to match the data.  Returns whether any did.
    fn update_child_count(&mut self, data: &impl ListIter<T>) -> bool {
        let len = self.children.len();
        match len.cmp(&data.data_len()) {
            Ordering::Greater => self.children.truncate(data.data_len()),
            Ordering::Less => data.for_each(|_, i| {
                if i >= len {
                    self.children.push(WidgetPod::new((self.closure)()));
                }
            }),
            Ordering::Equal => (),
        }
        len != data.data_len()
    }
}

/// Where each child goes, and the total size, for children of `sizes` placed in
/// rows no wider than `max_width`.
fn wrap_positions(sizes: &[Size], max_width: f64, spacing: f64) -> (Vec<Point>, Size) {
    let mut positions = Vec::with_capacity(sizes.len());
    let (mut x, mut y, mut row_height, mut width) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    for size in sizes {
        if x > 0.0 && x + size.width > max_width {
            x = 0.0;
            y += row_height + spacing;
            row_height = 0.0;
        }
        positions.push(Point::new(x, y));
        width = width.max(x + size.width);
        row_height = row_height.max(size.height);
        x += size.width + spacing;
    }
    (positions, Size::new(width, y + row_height))
}

impl<C: Data, T: ListIter<C>> Widget<T> for Wrap<C> {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut T, env: &Env) {
        let mut children = self.children.iter_mut();
        data.for_each_mut(|child_data, _| {
            if let Some(child) = children.next() {
                child.event(ctx, event, child_data, env);
            }
        });
    }

    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &T, env: &Env) {
        if let LifeCycle::WidgetAdded = event {
            if self.update_child_count(data) {
                ctx.children_changed();
            }
        }
        let mut children = self.children.iter_mut();
        data.for_each(|child_data, _| {
            if let Some(child) = children.next() {
                child.lifecycle(ctx, event, child_data, env);
            }
        });
    }

    fn update(&mut self, ctx: &mut UpdateCtx, _old_data: &T, data: &T, env: &Env) {
        let mut children = self.children.iter_mut();
        data.for_each(|child_data, _| {
            if let Some(child) = children.next() {
                child.update(ctx, child_data, env);
            }
        });
        if self.update_child_count(data) {
            ctx.children_changed();
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx, bc: &BoxConstraints, data: &T, env: &Env) -> Size {
        let child_bc = BoxConstraints::new(Size::ZERO, Size::new(bc.max().width, f64::INFINITY));
        let mut sizes = Vec::with_capacity(self.children.len());
        let mut children = self.children.iter_mut();
        data.for_each(|child_data, _| {
            if let Some(child) = children.next() {
                sizes.push(child.layout(ctx, &child_bc, child_data, env));
            }
        });
        let (positions, size) = wrap_positions(&sizes, bc.max().width, self.spacing);
        let mut paint_rect = Rect::ZERO;
        for (child, position) in self.children.iter_mut().zip(positions) {
            child.set_origin(ctx, position);
            paint_rect = paint_rect.union(child.paint_rect());
        }
        let size = bc.constrain(size);
        ctx.set_paint_insets(paint_rect - size.to_rect());
        size
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &T, env: &Env) {
        let mut children = self.children.iter_mut();
        data.for_each(|child_data, _| {
            if let Some(child) = children.next() {
                child.paint(ctx, child_data, env);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_to_the_width() {
        let sizes = [Size::new(40.0, 10.0); 5];
        // 40 + 10 + 40 + 10 + 40 = 140 fits in 150, the fourth starts a row.
        let (positions, size) = wrap_positions(&sizes, 150.0, 10.0);
        let rows: Vec<_> = positions.iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(
            rows,
            [
                (0.0, 0.0),
                (50.0, 0.0),
                (100.0, 0.0),
                (0.0, 20.0),
                (50.0, 20.0)
            ]
        );
        assert_eq!(size, Size::new(140.0, 30.0));
        // Too narrow for even one: one per row, never an empty row.
        let (positions, _) = wrap_positions(&sizes[..2], 30.0, 10.0);
        assert_eq!(positions[1], Point::new(0.0, 20.0));
    }
}
