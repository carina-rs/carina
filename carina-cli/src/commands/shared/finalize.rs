use crate::error::AppError;

/// Handle state finalization after a mutating command has observed execution.
///
/// Four cases:
/// 1. not cancelled + finalize Ok    -> Ok(())
/// 2. not cancelled + finalize Err   -> Err(finalize_err)
/// 3. cancelled     + finalize Ok    -> Err(Interrupted)
/// 4. cancelled     + finalize Err   -> Err(Interrupted) after logging warning
///
/// On cancellation, `Interrupted` takes precedence so the user sees the
/// interruption instead of a cleanup error. The finalize error is still logged.
pub(crate) fn handle_finalize_after_execute(
    finalize_result: Result<(), AppError>,
    cancelled: bool,
) -> Result<(), AppError> {
    if cancelled {
        if let Err(err) = finalize_result {
            eprintln!("warning: state save during cancellation also failed: {err}");
        }
        return Err(AppError::Interrupted);
    }

    finalize_result
}
