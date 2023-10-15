use owo_colors::{OwoColorize, Style, Styled};
use supports_color::Stream::Stderr;

pub trait ErrStyle {
    fn errstyle(&self, style: impl FnOnce(Style) -> Style) -> Styled<&Self>;
}

impl<D> ErrStyle for D {
    fn errstyle(&self, style: impl FnOnce(Style) -> Style) -> Styled<&Self> {
        self.style(get_errstyle(style))
    }
}

pub fn get_errstyle(style: impl FnOnce(Style) -> Style) -> Style {
    supports_color::on(Stderr)
        .filter(|f| f.has_basic)
        .map_or_else(Style::new, |_| style(Style::new()))
}

pub static SITE_NAME_STYLE: fn(Style) -> Style = Style::yellow;
pub static SITE_VAL_STYLE: fn(Style) -> Style = Style::blue;
pub static CONFIG_VAL_STYLE: fn(Style) -> Style = Style::purple;
pub static FILE_STYLE: fn(Style) -> Style = Style::cyan;
pub static SUCCESS_STYLE: fn(Style) -> Style = Style::green;
