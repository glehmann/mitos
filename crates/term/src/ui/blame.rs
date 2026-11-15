//! Commit details and copy actions for the primary cursor's blame.

use crate::compositor::DrawContext;
use theme::Theme;
use tui::{
    buffer::Buffer,
    text::{Line, Span, Text},
    widgets::Widget,
};
use ui_core::keymap::{KeymapState, Lookup};
use vcs::LineBlame;
use view::{
    features::blame::BlameDetailsUpdate,
    graphics::{Modifier, Rect},
    Editor,
};

use crate::{
    compositor::{Component, Compositor, Context, Event, EventResult},
    keymap::component::{action_hint, BlameAction, PopupAction},
    ui::{panel, text, Popup},
};

pub struct BlameDetails {
    keys: KeymapState,
    commit_id: String,
    change_id: Option<String>,
    unique_prefixes: (usize, usize),
    author: String,
    date: String,
    title: String,
    body: Option<String>,
    message: String,
}

impl BlameDetails {
    pub const ID: &'static str = "blame-details";

    /// A Jujutsu change is named by its change ID, which survives rewrites.
    fn id_name(change: bool) -> &'static str {
        if change {
            "change ID"
        } else {
            "commit ID"
        }
    }

    fn new(blame: LineBlame) -> Self {
        let title = blame.commit_title.unwrap_or_default();
        let body = blame.commit_body.filter(|body| !body.is_empty());
        let mut message = title.clone();
        if let Some(body) = &body {
            message.push_str("\n\n");
            message.push_str(body);
        }
        let author = match (blame.author_name, blame.author_email) {
            (Some(name), Some(email)) => format!("{name} <{email}>"),
            (Some(name), None) => name,
            (None, Some(email)) => email,
            (None, None) => "Unknown author".into(),
        };
        Self {
            keys: KeymapState::default(),
            commit_id: blame.commit_id,
            change_id: blame.change_id,
            unique_prefixes: blame.unique_prefixes,
            author,
            date: blame.commit_date.unwrap_or_else(|| "Unknown date".into()),
            title,
            body,
            message,
        }
    }

    fn text(&self, theme: Option<&Theme>) -> Text<'_> {
        let style = |scope| {
            theme
                .map(|theme| theme.get("ui.text").patch(theme.get(scope)))
                .unwrap_or_default()
        };
        let label = style("keyword");
        // Like `jj`, emphasize the shortest prefix that names the revision.
        let (unique, rest) = (style("constant"), style("comment"));
        let id = move |name, id, prefix| -> Line<'_> {
            let (start, end): (&str, &str) = str::split_at_checked(id, prefix).unwrap_or((id, ""));
            let spans = if start.is_empty() || end.is_empty() {
                vec![Span::styled(name, label), Span::styled(id, unique)]
            } else {
                vec![
                    Span::styled(name, label),
                    Span::styled(start, unique.add_modifier(Modifier::BOLD)),
                    Span::styled(end, rest),
                ]
            };
            Line::from(spans)
        };
        let (change_prefix, commit_prefix) = self.unique_prefixes;
        let mut lines: Vec<_> = self
            .change_id
            .iter()
            .map(|change| id("Change: ", change, change_prefix))
            .collect();
        lines.extend([
            id("Commit: ", &self.commit_id, commit_prefix),
            Line::from(vec![
                Span::styled("Author: ", label),
                Span::styled(&self.author, style("string")),
            ]),
            Line::from(vec![
                Span::styled("Date: ", label),
                Span::styled(&self.date, style("constant.numeric")),
            ]),
            Line::default(),
        ]);
        lines.extend(
            self.title
                .lines()
                .map(|line| Line::from(Span::styled(line, style("markup.heading.1")))),
        );
        // Commit messages are literal text, including Markdown and code fences.
        if let Some(body) = &self.body {
            lines.push(Line::default());
            lines.extend(
                body.lines()
                    .map(|line| Line::from(Span::styled(line, style("ui.text")))),
            );
        }
        Text::from(lines)
    }
}

pub fn show(
    editor: &mut Editor,
    compositor: &mut Compositor,
    update: BlameDetailsUpdate,
    config: &crate::config::Config,
) {
    let Some(result) = update.resolve(editor) else {
        return;
    };
    match result {
        Ok(blame) => {
            let id = BlameDetails::id_name(blame.change_id.is_some());
            let popup = Popup::new(BlameDetails::ID, BlameDetails::new(blame))
                .with_border(panel::PopupBorder::InlineBlame)
                .auto_close(true);
            compositor.replace_or_push(BlameDetails::ID, popup);
            let keys = &config.component_keys;
            editor.set_status(format!(
                "Blame: {} copies {id}; {} copies message; {} closes",
                action_hint(&keys.blame, &BlameAction::CopyCommitId),
                action_hint(&keys.blame, &BlameAction::CopyMessage),
                action_hint(&keys.popup, &PopupAction::Cancel),
            ));
        }
        Err(message) => editor.set_error(|| message),
    }
}

impl Component for BlameDetails {
    fn has_pending_keys(&self) -> bool {
        self.keys.is_pending()
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };
        match self
            .keys
            .lookup(&cx.config.current.component_keys.blame, *key)
        {
            Lookup::Pending => EventResult::Consumed(None),
            Lookup::Unbound => EventResult::Ignored(None),
            Lookup::Actions(actions) => {
                for action in actions {
                    let (text, label) = match action {
                        BlameAction::CopyCommitId => (
                            self.change_id.as_ref().unwrap_or(&self.commit_id),
                            Self::id_name(self.change_id.is_some()),
                        ),
                        BlameAction::CopyMessage => (&self.message, "commit message"),
                    };
                    match cx.editor.registers.write('+', vec![text.clone()]) {
                        Ok(()) => cx.editor.set_status(format!("Copied {label}")),
                        Err(err) => cx.editor.set_error(|| err.to_string()),
                    }
                }
                EventResult::Consumed(None)
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Buffer, cx: &mut DrawContext) {
        text::paragraph(self.text(Some(&cx.editor.theme)))
            .scroll((cx.scroll.unwrap_or_default() as u16, 0))
            .render(panel::content_area(area), surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let contents = text::paragraph(self.text(None));
        let max_text_width = viewport.0.saturating_sub(panel::HORIZONTAL_INSET).min(120);
        let (width, height) = text::required_size(&contents, max_text_width);
        Some((
            width + panel::HORIZONTAL_INSET,
            height + panel::VERTICAL_INSET,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_hints_follow_remapped_keys_and_sequences() {
        let source = "[keys.blame]\ny = 'unbound'\nY = 'unbound'\n[keys.blame.g]\nc = 'copy_commit_id'\nm = 'copy_message'".to_owned();
        let config = crate::config::Config::load(Ok(&source), Ok(String::new())).unwrap();
        assert_eq!(
            action_hint(&config.component_keys.blame, &BlameAction::CopyCommitId),
            "gc"
        );
        assert_eq!(
            action_hint(&config.component_keys.blame, &BlameAction::CopyMessage),
            "gm"
        );
    }
}
