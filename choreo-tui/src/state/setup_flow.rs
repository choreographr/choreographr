//! The staged account/model/reasoning choice: a pure state machine over the
//! facts a session must be bound to before it can chat.
//!
//! A session's account, its model and that model's reasoning effort are one
//! unit: an account with no model cannot answer, and a model with no effort is
//! a half-configured session. The flow therefore commits nothing until all
//! three are known — a single `SetSessionChoice` at the end — so abandoning it
//! at any point leaves the session exactly as it was, and there is no window in
//! which a client (or the persisted store) can observe a half-made choice.
//!
//! This module is the **decision core**: [`SetupFlow`] holds the phase and the
//! facts staged so far, [`SetupEvent`] is everything that can move it (a user
//! key or a daemon message), and a transition returns the next phase plus the
//! [`SetupStep`]s the caller must perform. Nothing here sends, opens or closes
//! anything, and nothing here waits: no timers, no polling, no I/O — the
//! driver in `connection/setup_flow.rs` performs the steps. That split is what
//! makes every rule below a total function of `(phase, event)` and unit-testable
//! without a daemon.

use super::App;
use choreo_proto::{ModelInfo, ReasoningCapability};

/// The effort slug a model with no reasoning support is committed with.
pub(crate) const OFF_EFFORT: &str = "off";

impl App {
    /// Drop the staged flow and the popups it opened, without navigating and
    /// without reporting anything. This is the state half of `abandon`: the
    /// driver in `connection/setup_flow.rs` decides *when* a flow's context has
    /// gone (the connection closed, the session changed, the client restarted),
    /// and this is what is left to do about it — nothing was committed, so
    /// nothing is half-done.
    pub(crate) fn abandon_setup_flow(&mut self) {
        self.setup_flow = None;
        self.model_selector.close();
        self.reasoning_dialog.close();
    }
}

/// The staged account/model/reasoning choice. Nothing is committed until the
/// last step, so an abandoned flow leaves the session untouched.
///
/// Each variant carries the facts that stay true while the phase lasts, so a
/// later step can read them from the state instead of threading them through
/// the driver: the account always, the model once one is chosen, and the model
/// ladder once the daemon has answered for it. The values a step needs *once*
/// (the suggestion a popup pre-highlights) travel on the [`SetupStep`] itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupFlow {
    /// An account was chosen; its model list is in flight.
    LoadingModels {
        /// The candidate account, which the session is NOT bound to yet.
        account: String,
    },
    /// The list is in hand and the model picker is open.
    ///
    /// `models` is the list the reply staged — the picker is a view over it and
    /// a live refresh replaces both, so a row the user can see is always a row
    /// this holds, and a chosen model can be checked against the list that
    /// offered it.
    ChoosingModel {
        /// The candidate account.
        account: String,
        /// The account's models, in the daemon's order.
        models: Vec<ModelInfo>,
    },
    /// A model was chosen; its reasoning levels are in flight.
    LoadingReasoning {
        /// The candidate account.
        account: String,
        /// The staged model.
        model: String,
    },
    /// The model has levels; the reasoning dialog is open.
    ///
    /// `capability` keeps the ladder the daemon reported, so the effort the
    /// user picks is validated against it here rather than trusted from the
    /// popup that drew it.
    ChoosingReasoning {
        /// The candidate account.
        account: String,
        /// The staged model.
        model: String,
        /// The model's reasoning levels.
        capability: ReasoningCapability,
    },
    /// `SetSessionChoice` is in flight; waiting for the daemon's confirmation.
    Committing {
        /// The account being committed.
        account: String,
    },
}

/// Everything that can advance the flow. Each variant is driven by a user key
/// or a daemon message; nothing here is polled or timed.
#[derive(Clone, Debug)]
pub(crate) enum SetupEvent {
    /// The `Models` reply answering the flow's own `ListModels`.
    Models {
        /// The account's models.
        models: Vec<ModelInfo>,
        /// The id the picker should pre-highlight (the daemon's suggestion).
        suggested: Option<String>,
        /// Whether the list is the daemon's unverified stand-in.
        provisional: bool,
    },
    /// A live `ModelsUpdated` push for an account whose list is already staged.
    ModelsRefreshed {
        /// The account the list belongs to.
        account: String,
        /// The freshly fetched models.
        models: Vec<ModelInfo>,
    },
    /// The `ModelReasoning` reply answering the flow's own query.
    Reasoned {
        /// The model's reasoning levels.
        capability: ReasoningCapability,
        /// The effort the dialog should pre-highlight.
        suggested: Option<String>,
    },
    /// The user chose a model in the picker (Enter or a row click).
    ChoseModel(String),
    /// The user chose an effort in the reasoning dialog (Enter).
    ChoseEffort(String),
    /// The attached session's account became this one — the commit landed.
    Committed {
        /// The account the daemon reported.
        account: String,
    },
    /// The user pressed Esc anywhere in the flow.
    Cancelled,
    /// A daemon failure the caller has established belongs to this flow. The
    /// caller holds the daemon's error text and surfaces it.
    Failed,
}

/// One thing the caller must do to advance or end the flow. The core names the
/// action and carries the payload that will not be in the state afterwards; the
/// driver owns the sending and the popups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupStep {
    /// Send `ListModels { account }` for the candidate account.
    RequestModels {
        /// The candidate account.
        account: String,
    },
    /// Open the model picker on `models`, pre-highlighting `suggested`.
    ShowModelPicker {
        /// The models to offer.
        models: Vec<ModelInfo>,
        /// The id to pre-highlight (already resolved against `models`).
        suggested: Option<String>,
        /// Whether the list is the daemon's unverified stand-in.
        provisional: bool,
    },
    /// Replace the open picker's list after a live fetch.
    RefreshModelPicker {
        /// The freshly fetched models.
        models: Vec<ModelInfo>,
    },
    /// Send `ModelReasoning { account, model }`.
    RequestReasoning {
        /// The candidate account.
        account: String,
        /// The staged model.
        model: String,
    },
    /// Open the reasoning dialog on `levels`, pre-highlighting `suggested`.
    ShowReasoningDialog {
        /// The model's effort levels.
        levels: Vec<String>,
        /// The level to pre-highlight (already resolved against `levels`).
        suggested: Option<String>,
    },
    /// Send `SetSessionChoice { account, model, effort }` — the atomic commit.
    Commit {
        /// The account to bind.
        account: String,
        /// The model to select.
        model: String,
        /// The reasoning effort to set.
        effort: String,
    },
    /// Close the model picker and the reasoning dialog.
    ClosePopups,
    /// Return to the AI-provider accounts page, refreshing the list so the user
    /// sees the accounts where they left off.
    ReturnToAccounts,
}

/// How a flow ended, for a transition that left no phase behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowEnd {
    /// The user aborted (or the flow's context vanished). Nothing was sent.
    Cancelled,
    /// The commit landed on the session.
    Completed,
    /// A failure ended the flow; the caller surfaces the daemon's error.
    Failed,
}

/// The result of feeding one event to the machine: the phase to install (`None`
/// once the flow has ended, exactly when `ended` is `Some`) and the steps to
/// perform.
#[derive(Clone, Debug)]
pub(crate) struct Transition {
    /// The flow after the transition, or `None` when it ended.
    pub(crate) next: Option<SetupFlow>,
    /// How it ended, when it did.
    pub(crate) ended: Option<FlowEnd>,
    /// What the caller must do next, in order.
    pub(crate) steps: Vec<SetupStep>,
}

impl Transition {
    /// Continue in `flow` after performing `steps`.
    fn next(flow: SetupFlow, steps: Vec<SetupStep>) -> Self {
        Self {
            next: Some(flow),
            ended: None,
            steps,
        }
    }

    /// End the flow with `end` after performing `steps`.
    fn ended(end: FlowEnd, steps: Vec<SetupStep>) -> Self {
        Self {
            next: None,
            ended: Some(end),
            steps,
        }
    }
}

impl SetupFlow {
    /// Stage `account` and ask for its model list. Nothing is written to the
    /// session: this only inspects the candidate account.
    pub(crate) fn begin(account: String) -> Transition {
        Transition::next(
            Self::LoadingModels {
                account: account.clone(),
            },
            vec![SetupStep::RequestModels { account }],
        )
    }

    /// The candidate account this flow is staging.
    pub(crate) fn account(&self) -> &str {
        match self {
            Self::LoadingModels { account }
            | Self::ChoosingModel { account, .. }
            | Self::LoadingReasoning { account, .. }
            | Self::ChoosingReasoning { account, .. }
            | Self::Committing { account } => account,
        }
    }

    /// Feed one event to the machine.
    ///
    /// The two flow-ending events that need no phase knowledge — Esc and a
    /// failure — are handled first, so no phase can forget them: a cancel and a
    /// failure both leave the session untouched by construction, and only the
    /// popups have to be cleaned up.
    pub(crate) fn step(self, event: SetupEvent) -> Transition {
        match event {
            SetupEvent::Cancelled => Transition::ended(
                FlowEnd::Cancelled,
                // Back to the accounts page: the flow was entered from there,
                // so cancelling returns the user to where they started.
                vec![SetupStep::ClosePopups, SetupStep::ReturnToAccounts],
            ),
            SetupEvent::Failed => Transition::ended(FlowEnd::Failed, vec![SetupStep::ClosePopups]),
            // Any other event is only meaningful to the phase that awaits it.
            event => match (self, event) {
                (
                    Self::LoadingModels { account },
                    SetupEvent::Models {
                        models,
                        suggested,
                        provisional,
                    },
                ) => {
                    // Resolve the daemon's suggestion against the list it came
                    // with (never trust a pre-highlight that is not a row), then
                    // fall back to the first row so the picker always opens on
                    // something.
                    let suggested = suggested
                        .filter(|id| models.iter().any(|m| &m.id == id))
                        .or_else(|| models.first().map(|m| m.id.clone()));
                    Transition::next(
                        Self::ChoosingModel {
                            account,
                            models: models.clone(),
                        },
                        vec![SetupStep::ShowModelPicker {
                            models,
                            suggested,
                            provisional,
                        }],
                    )
                }
                (flow @ Self::ChoosingModel { .. }, SetupEvent::Models { .. }) => {
                    // One request, one answer: a second `Models` reply is not
                    // this flow's, and must not move the cursor under the user.
                    Transition::next(flow, Vec::new())
                }
                (
                    Self::ChoosingModel { account, .. },
                    SetupEvent::ModelsRefreshed {
                        account: refreshed,
                        models,
                    },
                ) if account == refreshed => Transition::next(
                    Self::ChoosingModel {
                        account,
                        models: models.clone(),
                    },
                    vec![SetupStep::RefreshModelPicker { models }],
                ),
                (
                    Self::LoadingReasoning { account, model },
                    SetupEvent::Reasoned {
                        capability,
                        suggested,
                    },
                ) => {
                    if capability.available_effort_levels.is_empty() {
                        // A model with no levels has exactly one possible
                        // effort, so there is no choice to show: commit "off"
                        // without opening the dialog.
                        Transition::next(
                            Self::Committing {
                                account: account.clone(),
                            },
                            vec![
                                SetupStep::ClosePopups,
                                SetupStep::Commit {
                                    account,
                                    model,
                                    effort: OFF_EFFORT.to_string(),
                                },
                            ],
                        )
                    } else {
                        let suggested = suggested
                            .filter(|effort| {
                                capability
                                    .available_effort_levels
                                    .iter()
                                    .any(|level| level == effort)
                            })
                            .or_else(|| capability.available_effort_levels.first().cloned());
                        let levels = capability.available_effort_levels.clone();
                        Transition::next(
                            Self::ChoosingReasoning {
                                account,
                                model,
                                capability,
                            },
                            vec![SetupStep::ShowReasoningDialog { levels, suggested }],
                        )
                    }
                }
                (Self::ChoosingModel { account, models }, SetupEvent::ChoseModel(model)) => {
                    // Only a model the staged list serves may be staged: a click
                    // resolved against a stale window must never carry an id
                    // this account does not offer into the commit.
                    if !models.iter().any(|m| m.id == model) {
                        return Transition::next(
                            Self::ChoosingModel { account, models },
                            Vec::new(),
                        );
                    }
                    Transition::next(
                        Self::LoadingReasoning {
                            account: account.clone(),
                            model: model.clone(),
                        },
                        vec![SetupStep::RequestReasoning { account, model }],
                    )
                }
                (
                    Self::ChoosingReasoning {
                        account,
                        model,
                        capability,
                    },
                    SetupEvent::ChoseEffort(effort),
                ) => {
                    if !capability.available_effort_levels.contains(&effort) {
                        return Transition::next(
                            Self::ChoosingReasoning {
                                account,
                                model,
                                capability,
                            },
                            Vec::new(),
                        );
                    }
                    Transition::next(
                        Self::Committing {
                            account: account.clone(),
                        },
                        vec![
                            SetupStep::ClosePopups,
                            SetupStep::Commit {
                                account,
                                model,
                                effort,
                            },
                        ],
                    )
                }
                (flow, SetupEvent::Committed { account }) => {
                    // The daemon confirmed the session's account. Only the
                    // account this flow staged is the flow's own commit; an
                    // account set by anything else (a typed `/account`, another
                    // client) leaves the flow alone.
                    if flow.account() == account {
                        Transition::ended(FlowEnd::Completed, vec![SetupStep::ClosePopups])
                    } else {
                        Transition::next(flow, Vec::new())
                    }
                }
                // Every remaining (phase, event) pair is inert: the reply or key
                // belongs to a phase that has moved on. Reinstall the phase
                // unchanged rather than dropping the flow.
                (flow, _) => Transition::next(flow, Vec::new()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, levels: &[&str]) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            reasoning_levels: levels.iter().map(|l| (*l).to_string()).collect(),
        }
    }

    fn list() -> Vec<ModelInfo> {
        vec![
            model("claude-3", &["off", "low", "medium", "high"]),
            model("gpt-4o", &[]),
        ]
    }

    fn capability(levels: &[&str]) -> ReasoningCapability {
        ReasoningCapability {
            available_effort_levels: levels.iter().map(|l| (*l).to_string()).collect(),
        }
    }

    /// A flow advanced to `ChoosingModel` on the test list.
    fn choosing_model() -> SetupFlow {
        let begin = SetupFlow::begin("anthropic".to_string());
        assert_eq!(
            begin.steps,
            vec![SetupStep::RequestModels {
                account: "anthropic".to_string()
            }]
        );
        let transition = begin.next.expect("staged").step(SetupEvent::Models {
            models: list(),
            suggested: Some("gpt-4o".to_string()),
            provisional: false,
        });
        transition.next.expect("choosing")
    }

    /// A flow advanced to `LoadingReasoning` for `model`.
    fn loading_reasoning(id: &str) -> SetupFlow {
        choosing_model()
            .step(SetupEvent::ChoseModel(id.to_string()))
            .next
            .expect("loading reasoning")
    }

    fn choosing_reasoning() -> SetupFlow {
        loading_reasoning("claude-3")
            .step(SetupEvent::Reasoned {
                capability: capability(&["off", "low", "medium", "high"]),
                suggested: Some("medium".to_string()),
            })
            .next
            .expect("choosing reasoning")
    }

    fn committing() -> SetupFlow {
        choosing_reasoning()
            .step(SetupEvent::ChoseEffort("high".to_string()))
            .next
            .expect("committing")
    }

    /// Every phase the machine can be in, so each sweep-the-board rule is
    /// asserted for all of them.
    fn every_phase() -> Vec<SetupFlow> {
        vec![
            SetupFlow::LoadingModels {
                account: "anthropic".to_string(),
            },
            choosing_model(),
            loading_reasoning("claude-3"),
            choosing_reasoning(),
            committing(),
        ]
    }

    // ── the happy path ──────────────────────────────────────────────────

    #[test]
    fn choosing_an_account_only_asks_for_its_models() {
        let transition = SetupFlow::begin("anthropic".to_string());
        assert_eq!(
            transition.next,
            Some(SetupFlow::LoadingModels {
                account: "anthropic".to_string()
            })
        );
        assert_eq!(transition.ended, None);
        assert_eq!(
            transition.steps,
            vec![SetupStep::RequestModels {
                account: "anthropic".to_string()
            }],
            "nothing is written to the session before the last step"
        );
    }

    #[test]
    fn a_model_list_opens_the_picker_on_the_suggestion() {
        let transition = SetupFlow::begin("anthropic".to_string())
            .next
            .expect("staged")
            .step(SetupEvent::Models {
                models: list(),
                suggested: Some("gpt-4o".to_string()),
                provisional: false,
            });
        assert_eq!(
            transition.next,
            Some(SetupFlow::ChoosingModel {
                account: "anthropic".to_string(),
                models: list(),
            })
        );
        assert_eq!(
            transition.steps,
            vec![SetupStep::ShowModelPicker {
                models: list(),
                suggested: Some("gpt-4o".to_string()),
                provisional: false,
            }]
        );
    }

    #[test]
    fn a_missing_or_unknown_suggestion_falls_back_to_the_first_row() {
        for suggested in [None, Some("not-in-the-list".to_string())] {
            let transition = SetupFlow::begin("anthropic".to_string())
                .next
                .expect("staged")
                .step(SetupEvent::Models {
                    models: list(),
                    suggested,
                    provisional: false,
                });
            let SetupStep::ShowModelPicker { suggested, .. } =
                transition.steps.first().cloned().expect("the picker step")
            else {
                panic!("expected ShowModelPicker");
            };
            assert_eq!(suggested.as_deref(), Some("claude-3"));
        }
    }

    #[test]
    fn choosing_a_listed_model_asks_for_its_reasoning_levels() {
        let transition = choosing_model().step(SetupEvent::ChoseModel("claude-3".to_string()));
        assert_eq!(
            transition.next,
            Some(SetupFlow::LoadingReasoning {
                account: "anthropic".to_string(),
                model: "claude-3".to_string(),
            })
        );
        assert_eq!(
            transition.steps,
            vec![SetupStep::RequestReasoning {
                account: "anthropic".to_string(),
                model: "claude-3".to_string(),
            }],
            "a click or Enter only asks about the model, it selects nothing"
        );
    }

    #[test]
    fn an_unlisted_model_is_refused() {
        let transition = choosing_model().step(SetupEvent::ChoseModel("gpt-5".to_string()));
        assert_eq!(
            transition.next,
            Some(SetupFlow::ChoosingModel {
                account: "anthropic".to_string(),
                models: list(),
            })
        );
        assert_eq!(transition.steps, Vec::new(), "nothing is sent");
    }

    #[test]
    fn reasoning_levels_open_the_dialog_on_the_suggestion() {
        let transition = loading_reasoning("claude-3").step(SetupEvent::Reasoned {
            capability: capability(&["off", "low", "high"]),
            suggested: Some("low".to_string()),
        });
        assert!(matches!(
            transition.next,
            Some(SetupFlow::ChoosingReasoning { .. })
        ));
        assert_eq!(
            transition.steps,
            vec![SetupStep::ShowReasoningDialog {
                levels: vec!["off".to_string(), "low".to_string(), "high".to_string()],
                suggested: Some("low".to_string()),
            }]
        );
    }

    #[test]
    fn a_missing_effort_suggestion_falls_back_to_the_first_level() {
        let transition = loading_reasoning("claude-3").step(SetupEvent::Reasoned {
            capability: capability(&["off", "low"]),
            suggested: None,
        });
        let SetupStep::ShowReasoningDialog { suggested, .. } =
            transition.steps.first().cloned().expect("the dialog step")
        else {
            panic!("expected ShowReasoningDialog");
        };
        assert_eq!(suggested.as_deref(), Some("off"));
    }

    #[test]
    fn a_model_without_levels_commits_off_without_a_dialog() {
        let transition = loading_reasoning("gpt-4o").step(SetupEvent::Reasoned {
            capability: capability(&[]),
            suggested: None,
        });
        assert_eq!(
            transition.next,
            Some(SetupFlow::Committing {
                account: "anthropic".to_string()
            })
        );
        assert_eq!(
            transition.steps,
            vec![
                SetupStep::ClosePopups,
                SetupStep::Commit {
                    account: "anthropic".to_string(),
                    model: "gpt-4o".to_string(),
                    effort: OFF_EFFORT.to_string(),
                },
            ],
            "there is no choice to show, so none is shown"
        );
    }

    #[test]
    fn choosing_an_effort_commits_the_whole_choice() {
        let transition = choosing_reasoning().step(SetupEvent::ChoseEffort("high".to_string()));
        assert_eq!(
            transition.next,
            Some(SetupFlow::Committing {
                account: "anthropic".to_string()
            })
        );
        assert_eq!(
            transition.steps,
            vec![
                SetupStep::ClosePopups,
                SetupStep::Commit {
                    account: "anthropic".to_string(),
                    model: "claude-3".to_string(),
                    effort: "high".to_string(),
                },
            ]
        );
    }

    #[test]
    fn an_unsupported_effort_is_refused() {
        let transition = choosing_reasoning().step(SetupEvent::ChoseEffort("xhigh".to_string()));
        assert!(matches!(
            transition.next,
            Some(SetupFlow::ChoosingReasoning { .. })
        ));
        assert_eq!(transition.steps, Vec::new());
    }

    #[test]
    fn the_sessions_confirmed_account_completes_the_flow() {
        let transition = committing().step(SetupEvent::Committed {
            account: "anthropic".to_string(),
        });
        assert_eq!(transition.next, None);
        assert_eq!(transition.ended, Some(FlowEnd::Completed));
        assert_eq!(transition.steps, vec![SetupStep::ClosePopups]);
    }

    #[test]
    fn another_accounts_confirmation_leaves_the_flow_alone() {
        for phase in every_phase() {
            let transition = phase.step(SetupEvent::Committed {
                account: "openai".to_string(),
            });
            assert_eq!(
                transition.ended, None,
                "a foreign account set is not this flow's commit"
            );
            assert_eq!(transition.steps, Vec::new());
        }
    }

    // ── cancel and failure: nothing is sent, nothing is committed ────────

    #[test]
    fn esc_from_every_phase_closes_and_returns_to_the_accounts() {
        for phase in every_phase() {
            let transition = phase.step(SetupEvent::Cancelled);
            assert_eq!(transition.next, None);
            assert_eq!(transition.ended, Some(FlowEnd::Cancelled));
            assert_eq!(
                transition.steps,
                vec![SetupStep::ClosePopups, SetupStep::ReturnToAccounts],
                "the user goes back to where the flow started"
            );
            assert!(
                !transition
                    .steps
                    .iter()
                    .any(|s| matches!(s, SetupStep::Commit { .. })),
                "cancelling never commits"
            );
        }
    }

    #[test]
    fn a_failure_from_every_phase_ends_the_flow() {
        for phase in every_phase() {
            let transition = phase.step(SetupEvent::Failed);
            assert_eq!(transition.next, None, "a failure ends the flow");
            assert_eq!(transition.ended, Some(FlowEnd::Failed));
            assert_eq!(transition.steps, vec![SetupStep::ClosePopups]);
        }
    }

    #[test]
    fn abandoning_from_every_phase_closes_the_popups_and_stays_put() {
        // A vanished context (connection closed, session switched, client
        // restarted) is a state wipe, not a transition: nothing was committed,
        // so the flow is simply dropped with the popups it opened.
        for phase in every_phase() {
            let mut app = App::new();
            app.setup_flow = Some(phase);
            app.model_selector.open();
            app.reasoning_dialog.open(vec!["off".to_string()], None);
            app.abandon_setup_flow();
            assert_eq!(app.setup_flow, None);
            assert!(!app.model_selector.is_open());
            assert!(!app.reasoning_dialog.is_open());
        }
    }

    // ── replies that belong to a phase that has moved on ────────────────

    #[test]
    fn a_second_model_list_does_not_move_the_picker() {
        let transition = choosing_model().step(SetupEvent::Models {
            models: vec![model("other", &[])],
            suggested: Some("other".to_string()),
            provisional: true,
        });
        assert_eq!(
            transition.next,
            Some(SetupFlow::ChoosingModel {
                account: "anthropic".to_string(),
                models: list(),
            })
        );
        assert_eq!(transition.steps, Vec::new());
    }

    #[test]
    fn a_live_list_refresh_replaces_the_staged_models() {
        let refreshed = vec![model("claude-3", &["off"]), model("claude-4", &[])];
        let transition = choosing_model().step(SetupEvent::ModelsRefreshed {
            account: "anthropic".to_string(),
            models: refreshed.clone(),
        });
        assert_eq!(
            transition.next,
            Some(SetupFlow::ChoosingModel {
                account: "anthropic".to_string(),
                models: refreshed.clone(),
            }),
            "a model the live list added is choosable at once"
        );
        assert_eq!(
            transition.steps,
            vec![SetupStep::RefreshModelPicker { models: refreshed }]
        );
    }

    #[test]
    fn a_refresh_for_another_account_is_ignored() {
        let transition = choosing_model().step(SetupEvent::ModelsRefreshed {
            account: "openai".to_string(),
            models: vec![model("other", &[])],
        });
        assert_eq!(
            transition.next,
            Some(SetupFlow::ChoosingModel {
                account: "anthropic".to_string(),
                models: list(),
            })
        );
        assert_eq!(transition.steps, Vec::new());
    }

    #[test]
    fn a_reasoning_reply_before_the_model_is_chosen_is_ignored() {
        let transition = choosing_model().step(SetupEvent::Reasoned {
            capability: capability(&["off"]),
            suggested: None,
        });
        assert!(matches!(
            transition.next,
            Some(SetupFlow::ChoosingModel { .. })
        ));
        assert_eq!(transition.steps, Vec::new());
    }

    #[test]
    fn keys_reach_only_their_own_phase() {
        // A model chosen while the picker is not the phase, and an effort
        // chosen while the dialog is not: both inert.
        let transition =
            loading_reasoning("claude-3").step(SetupEvent::ChoseModel("gpt-4o".into()));
        assert!(matches!(
            transition.next,
            Some(SetupFlow::LoadingReasoning { .. })
        ));
        assert_eq!(transition.steps, Vec::new());

        let transition = choosing_model().step(SetupEvent::ChoseEffort("high".into()));
        assert!(matches!(
            transition.next,
            Some(SetupFlow::ChoosingModel { .. })
        ));
        assert_eq!(transition.steps, Vec::new());
    }

    #[test]
    fn the_flow_reports_the_account_it_is_staging() {
        for phase in every_phase() {
            assert_eq!(phase.account(), "anthropic");
        }
    }
}
