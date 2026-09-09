//! Translate Android's whole-field IME snapshots into winit composition events.
use android_activity::input::TextInputState;
use winit_core::event::Ime;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditorActionKey {
    Enter,
    Tab { reverse: bool },
}

impl EditorActionKey {
    pub(crate) fn from_android(action: android_activity::input::TextInputAction) -> Option<Self> {
        use android_activity::input::TextInputAction::*;
        match action {
            Next => Some(Self::Tab { reverse: false }),
            Previous => Some(Self::Tab { reverse: true }),
            Unspecified | None | Go | Search | Send | Done => Some(Self::Enter),
            _ => Option::None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ImeControl {
    pub capabilities: Option<winit_core::window::ImeCapabilities>,
    pub epoch: u64,
}

#[derive(Debug, Default)]
pub(crate) struct ImeState {
    committed: String,
    preedit: String,
    cursor: Option<(usize, usize)>,
    composing_text: Option<String>,
}

impl ImeState {
    /// Accept composition before an editor action starts a new insertion context.
    pub(crate) fn finish(&mut self) -> Vec<Ime> {
        let events = self.composing_text.take().map_or_else(Vec::new, |text| {
            self.update(&TextInputState { text, ..Default::default() })
        });
        *self = Self::default();
        events
    }

    pub(crate) fn update(&mut self, state: &TextInputState) -> Vec<Ime> {
        let mut events = Vec::new();
        let (preedit, cursor) = if let Some(span) = state.compose_region {
            let Some(start) = utf16_byte(&state.text, span.start) else {
                return events;
            };
            let Some(end) = utf16_byte(&state.text, span.end) else {
                return events;
            };
            if start > end {
                return events;
            }
            let cursor_start =
                utf16_byte(&state.text, state.selection.start).unwrap_or(end).clamp(start, end)
                    - start;
            let cursor_end =
                utf16_byte(&state.text, state.selection.end).unwrap_or(end).clamp(start, end)
                    - start;
            self.composing_text = Some(state.text.clone());
            (state.text[start..end].to_owned(), Some((cursor_start, cursor_end)))
        } else {
            self.composing_text = None;
            (String::new(), None)
        };
        let committed = &state.text;
        // A composing snapshot is not an accepted edit. In particular Android
        // can reopen already-committed words after Backspace. Removing them
        // now would lose text if the input connection is later cancelled.
        if state.compose_region.is_none() && *committed != self.committed {
            let prefix: usize = self
                .committed
                .chars()
                .zip(committed.chars())
                .take_while(|(old, new)| old == new)
                .map(|(ch, _)| ch.len_utf8())
                .sum();
            events.push(Ime::Preedit(String::new(), None));
            let before_bytes = self.committed.len() - prefix;
            if before_bytes != 0 {
                events.push(Ime::DeleteSurrounding { before_bytes, after_bytes: 0 });
            }
            // An empty commit terminates a deletion-only edit too, allowing
            // consumers to apply deletion and insertion as one transaction.
            events.push(Ime::Commit(committed[prefix..].to_owned()));
            self.committed = committed.clone();
            self.preedit.clear();
            self.cursor = None;
        }
        if preedit != self.preedit || cursor != self.cursor {
            events.push(Ime::Preedit(preedit.clone(), cursor));
            self.preedit = preedit;
            self.cursor = cursor;
        }
        events
    }
}

// GameTextInput exposes Java UTF-16 indices even though android-activity
// decodes the text buffer to Rust UTF-8. Never slice Rust text with Java offsets.
fn utf16_byte(text: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += character.len_utf16();
    }
    (units == offset).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use android_activity::input::TextSpan;

    use super::*;

    #[test]
    fn editor_actions_distinguish_acceptance_from_field_navigation() {
        use android_activity::input::TextInputAction;
        assert_eq!(
            EditorActionKey::from_android(TextInputAction::Done),
            Some(EditorActionKey::Enter)
        );
        assert_eq!(
            EditorActionKey::from_android(TextInputAction::Next),
            Some(EditorActionKey::Tab { reverse: false })
        );
        assert_eq!(
            EditorActionKey::from_android(TextInputAction::Previous),
            Some(EditorActionKey::Tab { reverse: true })
        );
    }

    #[test]
    fn reopening_composition_does_not_delete_accepted_text_before_commit() {
        let mut ime = ImeState::default();
        ime.update(&TextInputState {
            text: "hello ".into(),
            selection: TextSpan { start: 6, end: 6 },
            compose_region: None,
        });
        assert_eq!(
            ime.update(&TextInputState {
                text: "hello".into(),
                selection: TextSpan { start: 5, end: 5 },
                compose_region: Some(TextSpan { start: 0, end: 5 }),
            }),
            vec![Ime::Preedit("hello".into(), Some((5, 5)))]
        );
        assert_eq!(ime.finish(), vec![
            Ime::Preedit(String::new(), None),
            Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
            Ime::Commit(String::new()),
        ]);
    }

    #[test]
    fn composition_is_not_committed_until_finished_and_never_duplicated() {
        let mut ime = ImeState::default();
        let mut state = TextInputState {
            text: "你好".into(),
            selection: TextSpan { start: 2, end: 2 },
            compose_region: Some(TextSpan { start: 0, end: 2 }),
        };
        assert_eq!(ime.update(&state), vec![Ime::Preedit("你好".into(), Some((6, 6)))]);
        state.compose_region = None;
        assert_eq!(ime.update(&state), vec![
            Ime::Preedit(String::new(), None),
            Ime::Commit("你好".into())
        ]);
        assert!(ime.update(&state).is_empty());
    }

    #[test]
    fn autocorrection_replaces_committed_text_instead_of_duplicating_it() {
        let mut ime = ImeState::default();
        let mut state = TextInputState {
            text: "helllo ".into(),
            selection: TextSpan { start: 7, end: 7 },
            compose_region: None,
        };
        ime.update(&state);
        state.text = "hello ".into();
        state.selection = TextSpan { start: 6, end: 6 };
        assert_eq!(ime.update(&state), vec![
            Ime::Preedit(String::new(), None),
            Ime::DeleteSurrounding { before_bytes: 3, after_bytes: 0 },
            Ime::Commit("o ".into()),
        ]);
    }

    #[test]
    fn emoji_composition_uses_utf16_spans_and_cancellation_does_not_insert() {
        let mut ime = ImeState::default();
        let state = TextInputState {
            text: "😀".into(),
            selection: TextSpan { start: 2, end: 2 },
            compose_region: Some(TextSpan { start: 0, end: 2 }),
        };
        assert_eq!(ime.update(&state), vec![Ime::Preedit("😀".into(), Some((4, 4)))]);
        assert_eq!(ime.update(&TextInputState::default()), vec![Ime::Preedit(String::new(), None)]);
    }

    #[test]
    fn editor_action_commits_active_composition_before_resetting_context() {
        let mut ime = ImeState::default();
        ime.update(&TextInputState {
            text: "hello".into(),
            selection: TextSpan { start: 5, end: 5 },
            compose_region: Some(TextSpan { start: 0, end: 5 }),
        });
        assert_eq!(ime.finish(), vec![
            Ime::Preedit(String::new(), None),
            Ime::Commit("hello".into())
        ]);
        assert!(ime.update(&TextInputState::default()).is_empty());
    }
}
