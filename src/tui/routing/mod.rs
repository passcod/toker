//! Read-only routing policy page.

mod client;
mod model;
mod view;

use client::Client;
use model::State;

/// Page-owned state, including the bounded control worker.
pub(super) struct Page {
    client: Client,
    state: State,
}

impl Page {
    pub(super) fn new(port: u16) -> Page {
        let client = Client::new(port);
        let mut page = Page {
            client,
            state: State::default(),
        };
        page.refresh();
        page
    }

    pub(super) fn refresh(&mut self) {
        if self.state.begin_refresh() {
            self.client.refresh();
        }
    }

    pub(super) fn poll(&mut self) {
        while let Some(reply) = self.client.poll() {
            self.state.receive(reply);
        }
    }

    pub(super) fn move_view(&mut self, delta: isize) {
        self.state.move_view(delta);
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
    }

    pub(super) fn set_view(&mut self, view: usize) {
        let view = match view {
            0 => model::View::Presentations,
            1 => model::View::Routes,
            2 => model::View::Rewrites,
            _ => model::View::Status,
        };
        self.state.set_view(view);
    }

    pub(super) fn render(&self, frame: &mut ratatui::Frame, legend: bool) -> view::Drawn {
        view::render(frame, &self.state, legend)
    }
}
