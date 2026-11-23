use iced::advanced::{graphics, text::{self, Paragraph}};
use iced::widget::{rich_text, text::Span};
use iced::{Pixels, Size};

pub fn wrapped(title: &str, size: f32) -> iced::Element<'static, crate::AppMessage> {
    let spans = crate::typography::emoji_segments(title).map(|(part, emoji)| {
        Span::<()>::new(part.to_owned())
            .font(if emoji { crate::typography::EMOJI } else { crate::typography::semibold() })
    }).collect::<Vec<_>>();
    rich_text(spans)
        .size(size)
        .line_height(1.1)
        .wrapping(text::Wrapping::WordOrGlyph)
        .style(crate::appearance::ui_text(crate::appearance::Tone::Text))
        .into()
}

fn measured_width(value: &str, size: f32) -> f32 {
    graphics::text::Paragraph::with_text(text::Text {
        content: value, bounds: Size::new(1_000_000.0, size * 2.0), size: Pixels(size),
        line_height: text::LineHeight::Relative(1.0), font: crate::typography::semibold(),
        align_x: text::Alignment::Default, align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced, wrapping: text::Wrapping::None,
    }).min_width()
}

pub fn fit(value: &str, width: f32, size: f32) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let width = (width - 1.0).max(0.0);
    if measured_width(&value, size) <= width { return value; }
    if measured_width("…", size) > width { return String::new(); }
    let ends: Vec<_> = value.grapheme_indices(true).map(|(start,g)|start+g.len()).collect();
    let mut low=0; let mut high=ends.len();
    while low<high {
        let mid=(low+high).div_ceil(2);
        let candidate=format!("{}…",value[..ends[mid-1]].trim_end());
        if measured_width(&candidate,size)<=width { low=mid; } else { high=mid-1; }
    }
    let end=if low==0 {0} else {ends[low-1]};
    format!("{}…",value[..end].trim_end())
}

#[cfg(test)]
mod tests {
    #[test]
    fn chat_title_uses_its_natural_width_and_wraps_when_constrained() {
        use iced::advanced::{layout, widget::Tree};
        let renderer = crate::chat_render_tests::headless_renderer();
        let mut title = super::wrapped("Platform Engineering and Integration Support Team", 24.0);
        let mut tree = Tree::new(title.as_widget());
        let limits = |width| layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 88.0));
        let wide = title.as_widget_mut().layout(&mut tree, &renderer, &limits(1000.0));
        assert!(wide.size().width > 280.0);
        let narrow = title.as_widget_mut().layout(&mut tree, &renderer, &limits(260.0));
        assert!(narrow.size().width <= 260.0);
        assert!(narrow.size().height > wide.size().height);
    }

    #[test]
    fn long_group_title_fits_the_remaining_space_and_keeps_graphemes() {
        use std::borrow::Cow;
        static FONTS: std::sync::Once = std::sync::Once::new();
        FONTS.call_once(|| {
            let mut fonts=iced::advanced::graphics::text::font_system().write().unwrap();
            fonts.load_font(Cow::Borrowed(crate::typography::REGULAR_BYTES));
            fonts.load_font(Cow::Borrowed(crate::typography::SEMIBOLD_BYTES));
        });
        let value="[PROJETO] ENGENHARIA DE PLATAFORMA E INTEGRAÇÕES WWWW";
        for width in [0.0,18.0,80.0,160.0,300.0] {
            let result=super::fit(value,width,22.0);
            assert!(super::measured_width(&result,22.0)<=width+0.5,"{width}: {result}");
        }
        assert_eq!(super::fit("Grupo curto",500.0,22.0),"Grupo curto");
        use unicode_segmentation::UnicodeSegmentation;
        let value="👩🏽‍💻 e\u{301}quipe de integrações";
        let result=super::fit(value,110.0,22.0);
        let prefix=result.trim_end_matches('…');
        assert!(value.grapheme_indices(true).map(|(index,_)|index).chain(std::iter::once(value.len())).any(|end|end==prefix.len()));
    }
}
