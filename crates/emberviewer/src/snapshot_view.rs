//! The two snapshot windows: select branches and save them, or read a file and
//! put its values back.
//!
//! Both windows work on the tree of the active provider. They collect the
//! network commands that they need and give them to the app, which sends them.

use std::collections::{BTreeSet, HashMap};

use ember_proto::glow::Value;

use crate::model::{format_value, Kind, TreeModel};
use crate::net::NetCommand;
use crate::snapshot::{count_capturable, plan_all, Item, Plan, Snapshot};

/// How long to wait before a directory request is sent again.
const FETCH_RETRY_SECS: f64 = 3.0;
/// How many times to ask for the same directory before you stop.
const FETCH_MAX_TRIES: u8 = 4;
/// How many directory requests to send in one frame. This keeps the load on a
/// small device low.
const FETCH_PER_FRAME: usize = 24;
/// How many values to write in one step, and the pause between two steps. A
/// restore of a large branch thus does not flood the device.
const WRITE_BATCH: usize = 8;
const WRITE_PAUSE_SECS: f64 = 0.05;
/// How often to match the file against the live tree again.
const REPLAN_SECS: f64 = 0.3;
/// How often to walk the selected branches of a snapshot.
const WALK_SECS: f64 = 0.25;

/// Asks the device for the parts of the tree that a snapshot needs.
#[derive(Default)]
struct Prefetch {
    /// Path -> (time of the last request, number of requests).
    asked: HashMap<Vec<u32>, (f64, u8)>,
    /// Requests left for this frame.
    budget: usize,
}

impl Prefetch {
    fn begin(&mut self) {
        self.budget = FETCH_PER_FRAME;
    }

    /// Ask for the children of `path`. The answer comes as a document later.
    /// Returns true while an answer is still expected.
    fn ask(
        &mut self,
        path: &[u32],
        now: f64,
        as_matrix: bool,
        commands: &mut Vec<NetCommand>,
    ) -> bool {
        let (last, tries) = self
            .asked
            .get(path)
            .copied()
            .unwrap_or((f64::NEG_INFINITY, 0));
        if tries >= FETCH_MAX_TRIES {
            return false; // The node stays empty. Do not ask again.
        }
        if now - last < FETCH_RETRY_SECS || self.budget == 0 {
            return true;
        }
        self.budget -= 1;
        self.asked.insert(path.to_vec(), (now, tries + 1));
        commands.push(if as_matrix {
            NetCommand::GetMatrixDirectory(path.to_vec())
        } else {
            NetCommand::GetDirectory(path.to_vec())
        });
        true
    }
}

/// What the save window asks the app to do.
pub enum SaveAction {
    /// Nothing this frame.
    None,
    /// Write a snapshot of these branches to a file.
    Save {
        branches: Vec<Vec<u32>>,
        with_read_only: bool,
    },
}

/// The window that selects branches for a snapshot.
pub struct SaveWindow {
    /// The branches that the operator selected. A branch holds every parameter
    /// below it.
    selected: BTreeSet<Vec<u32>>,
    with_read_only: bool,
    prefetch: Prefetch,
    /// Time of the last walk of the selected branches.
    walked_at: f64,
    /// Result of that walk: parameters to keep, and nodes that wait for an
    /// answer.
    count: usize,
    waiting: usize,
    /// The window closes when this becomes false.
    pub open: bool,
}

impl SaveWindow {
    /// Open the window. `branch` preselects one branch of the tree.
    pub fn new(branch: Option<Vec<u32>>) -> Self {
        let mut selected = BTreeSet::new();
        if let Some(path) = branch {
            selected.insert(path);
        }
        SaveWindow {
            selected,
            with_read_only: false,
            prefetch: Prefetch::default(),
            walked_at: f64::NEG_INFINITY,
            count: 0,
            waiting: 0,
            open: true,
        }
    }

    /// Draw the window.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        tree: &TreeModel,
        provider: &str,
        commands: &mut Vec<NetCommand>,
    ) -> SaveAction {
        let now = ctx.input(|i| i.time);
        let mut action = SaveAction::None;
        let mut open = self.open;

        // Read the parts of the selected branches that the app does not hold
        // yet, so that the snapshot is complete. The walk goes over the whole
        // selection, so it runs only a few times a second.
        let branches: Vec<Vec<u32>> = self.selected.iter().cloned().collect();
        if now - self.walked_at >= WALK_SECS {
            self.walked_at = now;
            self.prefetch.begin();
            self.waiting = branches
                .iter()
                .map(|branch| prefetch_walk(tree, branch, &mut self.prefetch, now, commands))
                .sum();
            self.count = count_capturable(tree, &branches, self.with_read_only);
        }
        let (count, waiting) = (self.count, self.waiting);
        if waiting > 0 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(WALK_SECS));
        }

        egui::Window::new(format!("Snapshot · {provider}"))
            .open(&mut open)
            .resizable(true)
            .default_width(420.0)
            .default_height(460.0)
            .show(ctx, |ui| {
                ui.label(
                    "Select the branches to keep. The file holds the path of every \
                     parameter from the device root, so a restore finds them again.",
                );
                ui.add_space(4.0);
                let mut changed = false;
                ui.horizontal(|ui| {
                    changed |= ui
                        .checkbox(&mut self.with_read_only, "Include read-only")
                        .on_hover_text(
                            "Keep read-only parameters as a record. A restore cannot write them.",
                        )
                        .changed();
                    if ui.button("Clear selection").clicked() {
                        self.selected.clear();
                        changed = true;
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - 34.0).max(60.0))
                    .show(ui, |ui| {
                        if tree.roots.is_empty() {
                            ui.weak("The tree is empty. Wait for the device.");
                        }
                        for root in sorted(&tree.roots) {
                            changed |= branch_row(ui, tree, &root, false, &mut self.selected);
                        }
                    });
                if changed {
                    self.walked_at = f64::NEG_INFINITY; // Count again at once.
                }

                ui.separator();
                ui.horizontal(|ui| {
                    let ready = count > 0 && waiting == 0;
                    if ui
                        .add_enabled(count > 0, egui::Button::new("Save…"))
                        .on_hover_text(if ready {
                            "Write the snapshot to a file"
                        } else {
                            "The app still reads the tree. You can wait for a complete file."
                        })
                        .clicked()
                    {
                        action = SaveAction::Save {
                            branches: branches.clone(),
                            with_read_only: self.with_read_only,
                        };
                    }
                    ui.label(format!(
                        "{count} parameter(s) in {} branch(es)",
                        branches.len()
                    ));
                    if waiting > 0 {
                        ui.spinner();
                        ui.weak(format!("reading {waiting} node(s)…"));
                    }
                });
            });

        self.open = open;
        action
    }
}

/// Draw one element of the tree with a selection box, and its children below.
/// Returns true if the operator changed the selection.
fn branch_row(
    ui: &mut egui::Ui,
    tree: &TreeModel,
    path: &[u32],
    inherited: bool,
    selected: &mut BTreeSet<Vec<u32>>,
) -> bool {
    let Some(entry) = tree.get(path) else {
        return false;
    };
    let mut checked = inherited || selected.contains(path);
    let child_inherited = checked;
    let mut changed = false;

    if entry.kind.is_expandable() {
        let id = ui.make_persistent_id(("snapbranch", path));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                changed |= select_box(ui, path, inherited, &mut checked, selected);
                ui.label(egui::RichText::new(entry.label()).strong());
            })
            .body(|ui| {
                for child in sorted(&entry.children) {
                    changed |= branch_row(ui, tree, &child, child_inherited, selected);
                }
                if entry.children.is_empty() {
                    ui.weak("…");
                }
            });
    } else {
        ui.horizontal(|ui| {
            changed |= select_box(ui, path, inherited, &mut checked, selected);
            let writable = entry.is_writable();
            let mut name = egui::RichText::new(entry.label());
            if !writable {
                name = name.weak();
            }
            ui.label(name).on_hover_text(if writable {
                "writable"
            } else {
                "read-only - kept only with \"Include read-only\""
            });
            if let Some(value) = &entry.value {
                ui.weak(short(&format_value(value)));
            }
        });
    }
    changed
}

/// The selection box of one row. A row below a selected branch shows a box that
/// you cannot clear here. Returns true if the operator changed the selection.
fn select_box(
    ui: &mut egui::Ui,
    path: &[u32],
    inherited: bool,
    checked: &mut bool,
    selected: &mut BTreeSet<Vec<u32>>,
) -> bool {
    if inherited {
        ui.add_enabled(false, egui::Checkbox::new(checked, ""))
            .on_disabled_hover_text("A branch above this one is selected.");
        return false;
    }
    if !ui.checkbox(checked, "").changed() {
        return false;
    }
    if *checked {
        // The new branch holds every element below it.
        selected.retain(|p| !p.starts_with(path));
        selected.insert(path.to_vec());
    } else {
        selected.remove(path);
    }
    true
}

/// The window that puts the values of a snapshot back into a device.
pub struct RestoreWindow {
    /// The name of the file, for the operator.
    pub source: String,
    snapshot: Snapshot,
    items: Vec<Item>,
    /// One flag per item: whether the restore writes it.
    picked: Vec<bool>,
    /// The state of each item on the live device.
    plans: Vec<Plan>,
    planned_at: f64,
    hide_same: bool,
    prefetch: Prefetch,
    /// The writes that wait for their turn.
    queue: Vec<(Vec<u32>, Value)>,
    /// How many writes the current restore holds and has sent.
    total: usize,
    sent: usize,
    last_write: f64,
    report: Option<String>,
    pub open: bool,
}

impl RestoreWindow {
    /// Open the window for a snapshot that comes from `source`.
    pub fn new(snapshot: Snapshot, source: String) -> Self {
        let items = snapshot.items();
        let picked = vec![true; items.len()];
        RestoreWindow {
            source,
            snapshot,
            plans: Vec::new(),
            picked,
            items,
            planned_at: f64::NEG_INFINITY,
            hide_same: true,
            prefetch: Prefetch::default(),
            queue: Vec::new(),
            total: 0,
            sent: 0,
            last_write: 0.0,
            report: None,
            open: true,
        }
    }

    /// Draw the window and send one step of the restore.
    ///
    /// Returns the values that go to the device this frame. The app sends them
    /// and marks them as pending.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        tree: &TreeModel,
        target: &str,
        target_addr: &str,
        armed: bool,
        commands: &mut Vec<NetCommand>,
    ) -> Vec<(Vec<u32>, Value)> {
        let now = ctx.input(|i| i.time);
        if !armed {
            // The operator closed the safety lock. Stop a restore that runs.
            self.queue.clear();
        }
        // Match the file against the live tree again from time to time. The tree
        // grows while the app reads it, so more items become ready.
        if now - self.planned_at >= REPLAN_SECS {
            self.plans = plan_all(tree, &self.items);
            self.planned_at = now;
        }

        // Read the parts of the tree that the file needs.
        self.prefetch.begin();
        let mut waiting = 0;
        let mut wanted: BTreeSet<Vec<u32>> = BTreeSet::new();
        for plan in &self.plans {
            if let Plan::Loading { fetch } = plan {
                wanted.insert(fetch.clone());
            }
        }
        let mut stalled: BTreeSet<Vec<u32>> = BTreeSet::new();
        for path in &wanted {
            let as_matrix = tree.get(path).is_some_and(|e| e.matrix.is_some());
            if self.prefetch.ask(path, now, as_matrix, commands) {
                waiting += 1;
            } else {
                stalled.insert(path.clone());
            }
        }
        // The device does not answer for these nodes. Report the parameters
        // below them as missing, not as still loading.
        for plan in &mut self.plans {
            if let Plan::Loading { fetch } = plan {
                if stalled.contains(fetch) {
                    *plan = Plan::Missing;
                }
            }
        }
        if waiting > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        let mut open = self.open;
        let mut writes: Vec<(Vec<u32>, Value)> = Vec::new();
        let mut apply = false;
        let mut verify = false;

        egui::Window::new("Restore snapshot")
            .open(&mut open)
            .resizable(true)
            .default_width(640.0)
            .default_height(480.0)
            .show(ctx, |ui| {
                self.header(ui, target, target_addr);
                ui.separator();
                let counts = Counts::of(&self.plans, &self.picked);
                self.toolbar(ui, &counts);
                ui.separator();
                self.table(ui, (ui.available_height() - 34.0).max(60.0));
                ui.separator();
                ui.horizontal(|ui| {
                    let busy = !self.queue.is_empty();
                    if ui
                        .add_enabled(
                            counts.ready > 0 && !busy && armed,
                            egui::Button::new(format!("Apply {} value(s)", counts.ready)),
                        )
                        .on_hover_text("Write the selected values to the device")
                        .on_disabled_hover_text(if armed {
                            "Select the rows to write"
                        } else {
                            "The safety lock is closed. Open the padlock to write values."
                        })
                        .clicked()
                    {
                        apply = true;
                    }
                    if busy {
                        ui.add(
                            egui::ProgressBar::new(self.sent as f32 / self.total.max(1) as f32)
                                .desired_width(140.0)
                                .text(format!("{}/{}", self.sent, self.total)),
                        );
                    }
                    if ui
                        .add_enabled(self.total > 0 && !busy, egui::Button::new("Verify"))
                        .on_hover_text("Read the values back from the device")
                        .clicked()
                    {
                        verify = true;
                    }
                    if waiting > 0 {
                        ui.spinner();
                        ui.weak(format!("reading {waiting} node(s)…"));
                    }
                    if let Some(report) = &self.report {
                        ui.label(report.clone());
                    }
                });
            });

        if apply {
            self.queue = self
                .plans
                .iter()
                .zip(&self.picked)
                .filter_map(|(plan, picked)| match plan {
                    Plan::Write { path, value, .. } if *picked => {
                        Some((path.clone(), value.clone()))
                    }
                    _ => None,
                })
                .collect();
            self.queue.reverse(); // The queue is drained from the end.
            self.total = self.queue.len();
            self.sent = 0;
            self.report = None;
        }
        if verify {
            // Only the selected rows, so that a large file does not put a burst
            // of requests on the device.
            for (plan, picked) in self.plans.iter().zip(&self.picked) {
                match plan.path() {
                    Some(path) if *picked => commands.push(NetCommand::RefreshValue(path.to_vec())),
                    _ => {}
                }
            }
            self.planned_at = f64::NEG_INFINITY;
        }

        // Write one batch, then wait. This keeps a small device responsive.
        if !self.queue.is_empty() {
            if now - self.last_write >= WRITE_PAUSE_SECS {
                self.last_write = now;
                for _ in 0..WRITE_BATCH {
                    let Some(write) = self.queue.pop() else { break };
                    self.sent += 1;
                    writes.push(write);
                }
                if self.queue.is_empty() {
                    self.report = Some(format!("applied {} value(s)", self.sent));
                    self.planned_at = f64::NEG_INFINITY; // Show the new state.
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(WRITE_PAUSE_SECS));
        }

        self.open = open;
        writes
    }

    /// Where the file comes from, and a warning if the target is another device.
    fn header(&self, ui: &mut egui::Ui, target: &str, target_addr: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.strong(&self.source);
            ui.weak(format!(
                "· {} parameter(s) · from {} ({}) · {}",
                self.items.len(),
                if self.snapshot.provider.is_empty() {
                    "unknown"
                } else {
                    &self.snapshot.provider
                },
                self.snapshot.address,
                self.snapshot.created
            ));
        });
        let other_device =
            !self.snapshot.address.is_empty() && self.snapshot.address != target_addr;
        if other_device {
            ui.label(
                egui::RichText::new(format!(
                    "⚠ You are connected to {target} ({target_addr}). Check the list before you apply."
                ))
                .color(egui::Color32::from_rgb(230, 160, 30)),
            );
        }
    }

    /// The selection buttons and the counts.
    fn toolbar(&mut self, ui: &mut egui::Ui, counts: &Counts) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("All").clicked() {
                self.picked.iter_mut().for_each(|p| *p = true);
            }
            if ui.button("None").clicked() {
                self.picked.iter_mut().for_each(|p| *p = false);
            }
            if ui
                .button("Only changes")
                .on_hover_text("Select the values that differ from the device")
                .clicked()
            {
                for (picked, plan) in self.picked.iter_mut().zip(&self.plans) {
                    *picked = plan.is_writable();
                }
            }
            ui.checkbox(&mut self.hide_same, "Hide equal values");
            ui.separator();
            ui.label(format!("{} to write", counts.ready));
            if counts.blocked > 0 {
                ui.weak(format!("· {} read-only", counts.blocked));
            }
            if counts.missing > 0 {
                ui.weak(format!("· {} not found", counts.missing));
            }
            if counts.loading > 0 {
                ui.weak(format!("· {} loading", counts.loading));
            }
        });
    }

    /// The list of captured parameters.
    fn table(&mut self, ui: &mut egui::Ui, height: f32) {
        let rows: Vec<usize> = (0..self.items.len())
            .filter(|&i| !self.hide_same || !matches!(self.plans.get(i), Some(Plan::Same { .. })))
            .collect();
        let cell = ui.spacing().interact_size.y;
        // A row is one line plus the space below it, or the scroll area shows
        // the wrong number of rows.
        let row_height = cell + ui.spacing().item_spacing.y;
        // The theme puts the brand colour here (see `apply_theme`).
        let accent = ui.visuals().hyperlink_color;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(height)
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                for &i in &rows[range] {
                    let Some(plan) = self.plans.get(i) else {
                        continue;
                    };
                    let item = &self.items[i];
                    ui.horizontal(|ui| {
                        let writable = plan.is_writable();
                        ui.add_enabled(writable, egui::Checkbox::new(&mut self.picked[i], ""));
                        ui.add_sized(
                            [64.0, cell],
                            egui::Label::new(status_text(plan, accent)).truncate(),
                        );
                        ui.add_sized([260.0, cell], egui::Label::new(item.label()).truncate())
                            .on_hover_text(item.label());
                        ui.add_sized(
                            [120.0, cell],
                            egui::Label::new(
                                egui::RichText::new(short(&format_value(&item.value))).strong(),
                            )
                            .truncate(),
                        );
                        let current = match plan {
                            Plan::Write { current, .. } => current
                                .as_ref()
                                .map(|v| short(&format_value(v)))
                                .unwrap_or_else(|| "-".into()),
                            Plan::Same { .. } => "same".into(),
                            _ => String::new(),
                        };
                        ui.weak(current);
                    });
                }
            });
    }
}

/// How many items are in each state.
struct Counts {
    ready: usize,
    blocked: usize,
    missing: usize,
    loading: usize,
}

impl Counts {
    fn of(plans: &[Plan], picked: &[bool]) -> Counts {
        let mut counts = Counts {
            ready: 0,
            blocked: 0,
            missing: 0,
            loading: 0,
        };
        for (i, plan) in plans.iter().enumerate() {
            match plan {
                Plan::Write { .. } if picked.get(i).copied().unwrap_or(false) => counts.ready += 1,
                Plan::ReadOnly { .. } | Plan::BadType { .. } => counts.blocked += 1,
                Plan::Missing => counts.missing += 1,
                Plan::Loading { .. } => counts.loading += 1,
                _ => {}
            }
        }
        counts
    }
}

/// The state of one item, in colour. `accent` is the brand colour of the theme.
fn status_text(plan: &Plan, accent: egui::Color32) -> egui::RichText {
    let text = egui::RichText::new(plan.status()).small();
    match plan {
        Plan::Write { .. } => text.color(accent),
        Plan::Missing => text.color(egui::Color32::from_rgb(200, 80, 60)),
        _ => text.weak(),
    }
}

/// Ask for the parts of a branch that the app does not hold yet. Returns the
/// number of nodes that wait for an answer.
fn prefetch_walk(
    tree: &TreeModel,
    path: &[u32],
    prefetch: &mut Prefetch,
    now: f64,
    commands: &mut Vec<NetCommand>,
) -> usize {
    let Some(entry) = tree.get(path) else {
        return 0;
    };
    // A function holds no values, so a snapshot does not go into it.
    if !entry.kind.is_expandable() || entry.kind == Kind::Function {
        return 0;
    }
    if entry.children.is_empty() {
        return usize::from(prefetch.ask(path, now, entry.matrix.is_some(), commands));
    }
    entry
        .children
        .iter()
        .map(|child| prefetch_walk(tree, child, prefetch, now, commands))
        .sum()
}

/// Sibling paths in number order.
fn sorted(paths: &[Vec<u32>]) -> Vec<Vec<u32>> {
    let mut out = paths.to_vec();
    out.sort_by_key(|p| p.last().copied().unwrap_or(0));
    out
}

/// Cut a value down to one short line for a table cell.
fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() > 40 {
        let cut: String = line.chars().take(39).collect();
        format!("{cut}…")
    } else if line.len() < text.len() {
        format!("{line}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_keeps_one_line() {
        assert_eq!(short("hello"), "hello");
        assert_eq!(short("two\nlines"), "two…");
        assert_eq!(short(&"x".repeat(50)), format!("{}…", "x".repeat(39)));
    }

    #[test]
    fn siblings_sort_by_number() {
        let paths = vec![vec![1, 10], vec![1, 2], vec![1, 1]];
        assert_eq!(sorted(&paths), vec![vec![1, 1], vec![1, 2], vec![1, 10]]);
    }
}
