//! Linux keyboard listener using inputlib
//!
//! # Shutdown Behavior
//!
//! When dropped, the listener stops processing events. The underlying thread
//! remains alive (inputlib limitation) but becomes idle because inputlib::grab()
//! blocks indefinitely and cannot be interrupted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::error::Result;
use crate::platform::state::{BlockingHotkeys, ListenerState};
use crate::types::{KeyEvent, Modifiers};

use super::keycode::{inputlib_button_to_key, inputlib_key_to_key, inputlib_key_to_modifier, update_modifiers};
use crate::types::Key;

/// Internal listener state returned to KeyboardListener
pub(crate) struct LinuxListenerState {
    pub event_receiver: Receiver<KeyEvent>,
    pub thread_handle: Option<JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub blocking_hotkeys: Option<BlockingHotkeys>,
}

/// Spawn an inputlib-based keyboard listener for Linux
pub(crate) fn spawn(blocking_hotkeys: Option<BlockingHotkeys>) -> Result<LinuxListenerState> {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(ListenerState::new(tx, blocking_hotkeys.clone())));
    let running = Arc::new(AtomicBool::new(true));

    let thread_state = Arc::clone(&state);
    let thread_running = Arc::clone(&running);

    let handle = thread::spawn(move || {
        loop {
            let cb_state = Arc::clone(&thread_state);
            let cb_running = Arc::clone(&thread_running);

            let callback = move |event: inputlib::Event| -> Option<inputlib::Event> {
                if !cb_running.load(Ordering::SeqCst) {
                    return Some(event);
                }

                let mut should_block = false;

                if let Ok(mut state) = cb_state.lock() {
                    match event.event_type {
                        inputlib::EventType::KeyPress(inputlib_key) => {
                            if let Some(changed_modifier) = inputlib_key_to_modifier(inputlib_key) {
                                let prev_mods = state.current_modifiers;
                                state.current_modifiers =
                                    update_modifiers(state.current_modifiers, inputlib_key, true);

                                if state.current_modifiers != prev_mods {
                                    // Never block standalone modifier events. If the process
                                    // crashes or hangs while a modifier is blocked, the release
                                    // event never reaches the compositor — leaving the key
                                    // permanently "held" and the system unusable without reboot.
                                    // Only the non-modifier key in a combo should be blocked.

                                    let _ = state.event_sender.send(KeyEvent {
                                        modifiers: state.current_modifiers,
                                        key: None,
                                        is_key_down: true,
                                        changed_modifier: Some(changed_modifier),
                                    });
                                }
                            } else if let Some(key) = inputlib_key_to_key(inputlib_key) {
                                should_block = state.should_block(state.current_modifiers, Some(key));

                                let _ = state.event_sender.send(KeyEvent {
                                    modifiers: state.current_modifiers,
                                    key: Some(key),
                                    is_key_down: true,
                                    changed_modifier: None,
                                });
                            }
                        }
                        inputlib::EventType::KeyRelease(inputlib_key) => {
                            if let Some(changed_modifier) = inputlib_key_to_modifier(inputlib_key) {
                                let prev_mods = state.current_modifiers;
                                state.current_modifiers =
                                    update_modifiers(state.current_modifiers, inputlib_key, false);

                                if state.current_modifiers != prev_mods {
                                    let _ = state.event_sender.send(KeyEvent {
                                        modifiers: state.current_modifiers,
                                        key: None,
                                        is_key_down: false,
                                        changed_modifier: Some(changed_modifier),
                                    });
                                }
                            } else if let Some(key) = inputlib_key_to_key(inputlib_key) {
                                should_block = state.should_block(state.current_modifiers, Some(key));

                                let _ = state.event_sender.send(KeyEvent {
                                    modifiers: state.current_modifiers,
                                    key: Some(key),
                                    is_key_down: false,
                                    changed_modifier: None,
                                });
                            }
                        }
                        inputlib::EventType::ButtonPress(button) => {
                            if let Some(key) = inputlib_button_to_key(button) {
                                let is_common = matches!(key, Key::MouseLeft | Key::MouseRight);
                                if !is_common || !state.current_modifiers.is_empty() {
                                    let _ = state.event_sender.send(KeyEvent {
                                        modifiers: state.current_modifiers,
                                        key: Some(key),
                                        is_key_down: true,
                                        changed_modifier: None,
                                    });
                                }
                            }
                        }
                        inputlib::EventType::ButtonRelease(button) => {
                            if let Some(key) = inputlib_button_to_key(button) {
                                let is_common = matches!(key, Key::MouseLeft | Key::MouseRight);
                                if !is_common || !state.current_modifiers.is_empty() {
                                    let _ = state.event_sender.send(KeyEvent {
                                        modifiers: state.current_modifiers,
                                        key: Some(key),
                                        is_key_down: false,
                                        changed_modifier: None,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if should_block {
                    None
                } else {
                    Some(event)
                }
            };

            match inputlib::grab(callback) {
                Ok(()) => break,
                Err(e) => {
                    eprintln!("inputlib grab error: {:?}, retrying in 2s", e);
                    if !thread_running.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    });

    Ok(LinuxListenerState {
        event_receiver: rx,
        thread_handle: Some(handle),
        running,
        blocking_hotkeys,
    })
}
