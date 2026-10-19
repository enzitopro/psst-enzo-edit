use druid::{widget::prelude::*, Data, Point, WidgetPod};

/// Shows `overlay` floating over the bottom right corner of `inner`.  Unlike
/// `Overlay`, mouse input over the overlay doesn't reach `inner`, so clicks on
/// a floating panel don't also hit what's below it.
pub struct CornerOverlay<T, W, O> {
    inner: WidgetPod<T, W>,
    overlay: WidgetPod<T, O>,
    margin: f64,
}

impl<T, W, O> CornerOverlay<T, W, O>
where
    W: Widget<T>,
    O: Widget<T>,
{
    pub fn new(inner: W, overlay: O, margin: f64) -> Self {
        Self {
            inner: WidgetPod::new(inner),
            overlay: WidgetPod::new(overlay),
            margin,
        }
    }

    fn is_over_overlay(&self, pos: Point) -> bool {
        let rect = self.overlay.layout_rect();
        rect.area() > 0.0 && rect.contains(pos)
    }
}

impl<T, W, O> Widget<T> for CornerOverlay<T, W, O>
where
    T: Data,
    W: Widget<T>,
    O: Widget<T>,
{
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut T, env: &Env) {
        let blocked = match event {
            Event::MouseDown(mouse) | Event::MouseUp(mouse) | Event::Wheel(mouse) => {
                self.is_over_overlay(mouse.pos)
            }
            _ => false,
        };
        if !blocked {
            self.inner.event(ctx, event, data, env);
        }
        self.overlay.event(ctx, event, data, env);
    }

    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &T, env: &Env) {
        self.inner.lifecycle(ctx, event, data, env);
        self.overlay.lifecycle(ctx, event, data, env);
    }

    fn update(&mut self, ctx: &mut UpdateCtx, _old_data: &T, data: &T, env: &Env) {
        self.inner.update(ctx, data, env);
        self.overlay.update(ctx, data, env);
    }

    fn layout(&mut self, ctx: &mut LayoutCtx, bc: &BoxConstraints, data: &T, env: &Env) -> Size {
        let size = self.inner.layout(ctx, bc, data, env);
        self.inner.set_origin(ctx, Point::ORIGIN);
        let overlay_bc = BoxConstraints::new(Size::ZERO, size);
        let overlay_size = self.overlay.layout(ctx, &overlay_bc, data, env);
        let origin = Point::new(
            (size.width - overlay_size.width - self.margin).max(0.0),
            (size.height - overlay_size.height - self.margin).max(0.0),
        );
        self.overlay.set_origin(ctx, origin);
        size
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &T, env: &Env) {
        self.inner.paint(ctx, data, env);
        self.overlay.paint(ctx, data, env);
    }
}
