use anyhow::Context as _;
use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, actions};
use language::{Buffer, LanguageServerId};
use picker::{Picker, PickerDelegate};
use project::LspStore;
use std::{collections::HashMap, sync::Arc};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Workspace};

actions!(
    lsp_workspace_command,
    [
        /// Toggles the lsp workspace command selector
        Toggle
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(LspWorkspaceCommandSelector::register)
        .detach();
}

pub struct LspWorkspaceCommandSelector {
    picker: Entity<Picker<LspWorkspaceCommandSelectorDelegate>>,
}

impl LspWorkspaceCommandSelector {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(move |workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, window, cx);
        });
    }

    fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<()> {
        let (_, buffer, _) = workspace
            .active_item(cx)?
            .act_as::<Editor>(cx)?
            .read(cx)
            .active_excerpt(cx)?;
        let project = workspace.project().clone();
        let lsp_store = project.read(cx).lsp_store().clone();

        workspace.toggle_modal(window, cx, move |window, cx| {
            LspWorkspaceCommandSelector::new(buffer, window, cx, lsp_store)
        });

        Some(())
    }

    fn new(
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
        lsp_store: Entity<LspStore>,
    ) -> Self {
        let delegate = LspWorkspaceCommandSelectorDelegate::new(
            cx.entity().downgrade(),
            buffer,
            cx,
            lsp_store,
        );

        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for LspWorkspaceCommandSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for LspWorkspaceCommandSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for LspWorkspaceCommandSelector {}
impl ModalView for LspWorkspaceCommandSelector {}

pub struct LspWorkspaceCommandSelectorDelegate {
    command_selector: WeakEntity<LspWorkspaceCommandSelector>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    commands: HashMap<String, (String, LanguageServerId)>,
    lsp_store: Entity<LspStore>,
}

impl LspWorkspaceCommandSelectorDelegate {
    fn new(
        command_selector: WeakEntity<LspWorkspaceCommandSelector>,
        buffer: Entity<Buffer>,
        cx: &mut App,
        lsp_store: Entity<LspStore>,
    ) -> Self {
        let mut commands = HashMap::new();

        lsp_store.update(cx, |store, cx| {
            buffer.update(cx, |buffer, cx| {
                let language_servers = store.language_servers_for_local_buffer(buffer, cx);

                for (_adaptor, language_server) in language_servers {
                    let current_commands = language_server
                        .capabilities()
                        .execute_command_provider
                        .map_or_else(|| vec![], |opt| opt.commands);

                    for command in current_commands {
                        commands.insert(
                            format!("{}: {}", language_server.name(), command),
                            (command, language_server.server_id()),
                        );
                    }
                }
            })
        });

        let candidates = commands
            .keys()
            .enumerate()
            .map(|(idx, command)| StringMatchCandidate::new(idx, &command))
            .collect::<Vec<_>>();

        Self {
            command_selector,
            candidates,
            commands,
            matches: vec![],
            selected_index: 0,
            lsp_store,
        }
    }
}

impl PickerDelegate for LspWorkspaceCommandSelectorDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select an lsp workspace command to execute…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let matched_string = &self.candidates[mat.candidate_id].string;

            if let Some(v) = self.commands.get(matched_string) {
                let (command, language_server_id) = v.clone();
                if let Some(language_server) = self
                    .lsp_store
                    .read(cx)
                    .language_server_for_id(language_server_id.clone())
                {
                    cx.spawn_in(window, async move |_, _| {
                        language_server
                            .request::<lsp::request::ExecuteCommand>(lsp::ExecuteCommandParams {
                                command: command.clone(),
                                arguments: vec![],
                                ..Default::default()
                            })
                            .await
                            .into_response()
                            .context("execute lsp workspace command")
                    })
                    .detach_and_log_err(cx);
                }
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.command_selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = &self.matches[ix];
        let label = mat.string.clone();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(label, mat.positions.clone())),
        )
    }
}
