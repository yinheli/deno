// Copyright 2018-2025 the Deno authors. MIT license.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use console_static_text::ConsoleStaticText;
use deno_core::parking_lot::Mutex;
use deno_core::unsync::spawn_blocking;
use deno_runtime::ops::tty::ConsoleSize;
use once_cell::sync::Lazy;

use crate::util::console::console_size;

/// Renders text that will be displayed stacked in a
/// static place on the console.
pub trait DrawThreadRenderer: Send + Sync + std::fmt::Debug {
  fn render(&self, data: &ConsoleSize) -> String;
}

/// Draw thread guard. Keep this alive for the duration
/// that you wish the entry to be drawn for. Once it is
/// dropped, then the entry will be removed from the draw
/// thread.
#[derive(Debug)]
pub struct DrawThreadGuard(u16);

impl Drop for DrawThreadGuard {
  fn drop(&mut self) {
    DrawThread::finish_entry(self.0)
  }
}

#[derive(Debug, Clone)]
struct InternalEntry {
  id: u16,
  renderer: Arc<dyn DrawThreadRenderer>,
}

#[derive(Debug)]
struct InnerState {
  // this ensures only one actual draw thread is running
  drawer_id: usize,
  hide_count: usize,
  has_draw_thread: bool,
  next_entry_id: u16,
  entries: Vec<InternalEntry>,
}

impl InnerState {
  pub fn should_exit_draw_thread(&self, drawer_id: usize) -> bool {
    self.drawer_id != drawer_id || self.entries.is_empty()
  }
}

struct GlobalState {
  state: Mutex<InnerState>,
  static_text: ConsoleStaticText,
}

static GLOBAL_STATE: Lazy<Arc<GlobalState>> = Lazy::new(|| {
  Arc::new(GlobalState {
    state: Mutex::new(InnerState {
      drawer_id: 0,
      hide_count: 0,
      has_draw_thread: false,
      entries: Vec::new(),
      next_entry_id: 0,
    }),
    static_text: ConsoleStaticText::new(|| {
      let size = console_size().unwrap();
      console_static_text::ConsoleSize {
        cols: Some(size.cols as u16),
        rows: Some(size.rows as u16),
      }
    }),
  })
});

static IS_TTY_WITH_CONSOLE_SIZE: Lazy<bool> = Lazy::new(|| {
  std::io::stderr().is_terminal()
    && console_size()
      .map(|s| s.cols > 0 && s.rows > 0)
      .unwrap_or(false)
});

/// The draw thread is responsible for rendering multiple active
/// `DrawThreadRenderer`s to stderr. It is global because the
/// concept of stderr in the process is also a global concept.
#[derive(Clone, Debug)]
pub struct DrawThread;

impl DrawThread {
  /// Is using a draw thread supported.
  pub fn is_supported() -> bool {
    // don't put the log level in the lazy because the
    // log level may change as the application runs
    log::log_enabled!(log::Level::Info) && *IS_TTY_WITH_CONSOLE_SIZE
  }

  /// Adds a renderer to the draw thread.
  pub fn add_entry(renderer: Arc<dyn DrawThreadRenderer>) -> DrawThreadGuard {
    let global_state = &*GLOBAL_STATE;
    let mut state = global_state.state.lock();
    let id = state.next_entry_id;
    state.entries.push(InternalEntry { id, renderer });

    if state.next_entry_id == u16::MAX {
      state.next_entry_id = 0;
    } else {
      state.next_entry_id += 1;
    }

    Self::maybe_start_draw_thread(&mut state);

    DrawThreadGuard(id)
  }

  /// Hides the draw thread.
  pub fn hide() {
    let global_state = &*GLOBAL_STATE;
    let is_showing = {
      let mut state = global_state.state.lock();
      let is_showing = state.has_draw_thread && state.hide_count == 0;
      state.hide_count += 1;
      is_showing
    };

    if is_showing {
      // Clear it on the current thread in order to stop it from
      // showing immediately. Also, don't stop the draw thread here
      // because the calling code might be called from outside a
      // tokio runtime and when it goes to start the thread on the
      // thread pool it might panic.
      global_state.static_text.eprint_clear();
    }
  }

  /// Shows the draw thread if it was previously hidden.
  pub fn show() {
    let global_state = &*GLOBAL_STATE;
    let mut state = global_state.state.lock();
    if state.hide_count > 0 {
      state.hide_count -= 1;
    }
  }

  fn finish_entry(entry_id: u16) {
    let global_state = &*GLOBAL_STATE;
    let should_clear = {
      let mut state = global_state.state.lock();
      if let Some(index) =
        state.entries.iter().position(|e| e.id == entry_id)
      {
        state.entries.remove(index);

        if state.entries.is_empty() && state.has_draw_thread {
          // bump the drawer id to exit the draw thread
          state.drawer_id += 1;
          state.has_draw_thread = false;
          true // should_clear
        } else {
          false
        }
      } else {
        false
      }
    };

    if should_clear {
      global_state.static_text.eprint_clear();
    }
  }

  fn maybe_start_draw_thread(state: &mut InnerState) {
    if state.has_draw_thread
      || state.entries.is_empty()
      || !DrawThread::is_supported()
    {
      return;
    }

    state.drawer_id += 1;
    state.has_draw_thread = true;

    let drawer_id = state.drawer_id;
    spawn_blocking(move || {
      let mut previous_size = console_size();
      loop {
        let mut delay_ms = 120;
        {
          // Get the entries to render.
          let maybe_entries = {
            let global_state = &*GLOBAL_STATE;
            let state = global_state.state.lock();
            if state.should_exit_draw_thread(drawer_id) {
              break;
            }
            let should_display = state.hide_count == 0;
            should_display.then(|| state.entries.clone())
          };

          if let Some(entries) = maybe_entries {
            // this should always be set, but have the code handle
            // it not being for some reason
            let size = console_size();

            // Call into the renderers outside the lock to prevent a potential
            // deadlock between our internal state lock and the renderers
            // internal state lock.
            //
            // Example deadlock if this code didn't do this:
            // 1. Other thread - Renderer - acquired internal lock to update state
            // 2. This thread  - Acquired internal state
            // 3. Other thread - Renderer - drops DrawThreadGuard
            // 4. This thread - Calls renderer.render within internal lock,
            //    which attempts to acquire the other thread's Render's internal
            //    lock causing a deadlock
            let mut text = String::new();
            if size != previous_size {
              // means the user is actively resizing the console...
              // wait a little bit until they stop resizing
              previous_size = size;
              delay_ms = 200;
            } else if let Some(size) = size {
              let mut should_new_line_next = false;
              for entry in entries {
                let new_text = entry.renderer.render(&size);
                if should_new_line_next && !new_text.is_empty() {
                  text.push('\n');
                }
                should_new_line_next = !new_text.is_empty();
                text.push_str(&new_text);
              }

              // now reacquire the lock, ensure we should still be drawing, then
              // output the text
              {
                let global_state = &*GLOBAL_STATE;
                let mut state = global_state.state.lock();
                if state.should_exit_draw_thread(drawer_id) {
                  break;
                }
                global_state.static_text.eprint_with_size(
                  &text,
                  console_static_text::ConsoleSize {
                    cols: Some(size.cols as u16),
                    rows: Some(size.rows as u16),
                  },
                );
              }
            }
          }
        }

        std::thread::sleep(Duration::from_millis(delay_ms));
      }
    });
  }
}