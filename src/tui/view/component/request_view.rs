use crate::{
    http::{RequestId, RequestRecord},
    tui::{
        input::Action,
        message::Message,
        view::{
            common::{
                actions::ActionsModal,
                header_table::HeaderTable,
                text_window::{TextWindow, TextWindowProps},
            },
            draw::{Draw, DrawMetadata, Generate, ToStringGenerate},
            event::{Event, EventHandler, Update},
            state::StateCell,
            Component, ViewContext,
        },
    },
    util::MaybeStr,
};
use derive_more::Display;
use ratatui::{layout::Layout, prelude::Constraint, Frame};
use std::sync::Arc;
use strum::{EnumCount, EnumIter};

/// Display rendered HTTP request state. The request could still be in flight,
/// it just needs to have been built successfully.
#[derive(Debug, Default)]
pub struct RequestView {
    state: StateCell<RequestId, State>,
}

pub struct RequestViewProps {
    pub request: Arc<RequestRecord>,
}

/// Inner state, which should be reset when request changes
#[derive(Debug)]
struct State {
    /// Store pointer to the request, so we can access it in the update step
    request: Arc<RequestRecord>,
    /// Persist the request body to track view state. `None` only if request
    /// doesn't have a body
    body: Option<Component<TextWindow<String>>>,
}

/// Items in the actions popup menu
#[derive(
    Copy, Clone, Debug, Default, Display, EnumCount, EnumIter, PartialEq,
)]
enum MenuAction {
    #[default]
    #[display("Edit Collection")]
    EditCollection,
    #[display("Copy URL")]
    CopyUrl,
    #[display("Copy Body")]
    CopyBody,
}

impl ToStringGenerate for MenuAction {}

impl EventHandler for RequestView {
    fn update(&mut self, event: Event) -> Update {
        if let Some(Action::OpenActions) = event.action() {
            ViewContext::open_modal_default::<ActionsModal<MenuAction>>()
        } else if let Some(action) = event.local::<MenuAction>() {
            match action {
                MenuAction::EditCollection => {
                    ViewContext::send_message(Message::CollectionEdit)
                }
                MenuAction::CopyUrl => {
                    if let Some(state) = self.state.get() {
                        ViewContext::send_message(Message::CopyText(
                            state.request.url.to_string(),
                        ))
                    }
                }
                MenuAction::CopyBody => {
                    // Copy exactly what the user sees. Currently requests
                    // don't support formatting/querying but that could
                    // change
                    if let Some(body) = self.state.get().and_then(|state| {
                        Some(state.body.as_ref()?.data().text().clone())
                    }) {
                        ViewContext::send_message(Message::CopyText(body));
                    }
                }
            }
        } else {
            return Update::Propagate(event);
        }
        Update::Consumed
    }

    fn children(&mut self) -> Vec<Component<&mut dyn EventHandler>> {
        if let Some(body) =
            self.state.get_mut().and_then(|state| state.body.as_mut())
        {
            vec![body.as_child()]
        } else {
            vec![]
        }
    }
}

impl Draw<RequestViewProps> for RequestView {
    fn draw(
        &self,
        frame: &mut Frame,
        props: RequestViewProps,
        metadata: DrawMetadata,
    ) {
        let state = self.state.get_or_update(props.request.id, || State {
            request: Arc::clone(&props.request),
            body: props.request.body.as_ref().map(|body| {
                TextWindow::new(format!("{:#}", MaybeStr(body))).into()
            }),
        });

        let [url_area, headers_area, body_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(props.request.headers.len() as u16 + 2),
            Constraint::Min(0),
        ])
        .areas(metadata.area());

        // This can get cut off which is jank but there isn't a good fix. User
        // can copy the URL to see the full thing
        frame.render_widget(props.request.url.to_string(), url_area);
        frame.render_widget(
            HeaderTable {
                headers: &props.request.headers,
            }
            .generate(),
            headers_area,
        );
        if let Some(body) = &state.body {
            body.draw(frame, TextWindowProps::default(), body_area, true);
        }
    }
}
