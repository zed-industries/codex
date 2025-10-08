use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;

use crate::render::Insets;
use crate::render::RectExt as _;

pub trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
}

impl<R: Renderable + 'static> From<R> for Box<dyn Renderable> {
    fn from(value: R) -> Self {
        Box::new(value)
    }
}

impl Renderable for () {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}
    fn desired_height(&self, _width: u16) -> u16 {
        0
    }
}

impl Renderable for &str {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl Renderable for String {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl<'a> Renderable for Line<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        WidgetRef::render_ref(self, area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl<'a> Renderable for Paragraph<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.line_count(width) as u16
    }
}

impl<R: Renderable> Renderable for Option<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(renderable) = self {
            renderable.render(area, buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        if let Some(renderable) = self {
            renderable.desired_height(width)
        } else {
            0
        }
    }
}

impl<R: Renderable> Renderable for Arc<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_ref().render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.as_ref().desired_height(width)
    }
}

pub struct ColumnRenderable {
    children: Vec<Box<dyn Renderable>>,
}

impl Renderable for ColumnRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        for child in &self.children {
            let child_area = Rect::new(area.x, y, area.width, child.desired_height(area.width))
                .intersection(area);
            if !child_area.is_empty() {
                child.render(child_area, buf);
            }
            y += child_area.height;
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.children
            .iter()
            .map(|child| child.desired_height(width))
            .sum()
    }
}

impl ColumnRenderable {
    pub fn new(children: impl IntoIterator<Item = Box<dyn Renderable>>) -> Self {
        Self {
            children: children.into_iter().collect(),
        }
    }
}

pub struct InsetRenderable {
    child: Box<dyn Renderable>,
    insets: Insets,
}

impl Renderable for InsetRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.child.render(area.inset(self.insets), buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.child
            .desired_height(width - self.insets.left - self.insets.right)
            + self.insets.top
            + self.insets.bottom
    }
}

impl InsetRenderable {
    pub fn new(child: impl Into<Box<dyn Renderable>>, insets: Insets) -> Self {
        Self {
            child: child.into(),
            insets,
        }
    }
}
