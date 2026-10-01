//! The ledger of manual entries, overrides and locks that sits beside the
//! computed timesheet. This is the contract: `apply` is the single point the
//! computation calls, and it changes nothing until the ledger is implemented.
// Called by the timesheet core once it exists.
#![allow(dead_code)]

use anyhow::Result;

use super::model::Timesheet;

/// Applies overrides, manual entries and locks to a computed timesheet. A
/// no-op until the ledger lands: every entry stays as computed.
pub(crate) fn apply(_timesheet: &mut Timesheet) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timesheet::model::{TimesheetMethodology, TimesheetSettings, TimesheetWindow};

    #[test]
    fn applying_the_empty_ledger_changes_nothing() {
        let mut timesheet = Timesheet {
            window: TimesheetWindow::default(),
            settings: TimesheetSettings::default(),
            entries: Vec::new(),
            dropped: Vec::new(),
            cross_check: Vec::new(),
            warnings: Vec::new(),
            methodology: TimesheetMethodology {
                status: "suggested",
                split_rule: String::new(),
                rounding: String::new(),
            },
        };
        let before = timesheet.clone();
        apply(&mut timesheet).unwrap();
        assert_eq!(before, timesheet);
    }
}
