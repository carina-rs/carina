#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
# An optional checker path allows mutation checks against temporary copies.
checker="${1:-$repo_root/scripts/check-no-wall-clock-test-assertions.sh}"
fixtures=$(mktemp -d)
trap 'rm -rf "$fixtures"' EXIT
mkdir -p "$fixtures/tests"

reject() {
  if bash "$checker" "$fixtures/$1" >"$fixtures/output" 2>&1; then
    echo "checker unexpectedly accepted $1" >&2
    exit 1
  fi
  if ! grep -q "$1:" "$fixtures/output"; then
    cat "$fixtures/output" >&2
    exit 1
  fi
}

# Each of these fixtures depends on one independent checker path. Keep them
# before broader cases so a mutation reports the precise fixture it misses.
cat >"$fixtures/tests/recv_timeout.rs" <<'RS'
#[test]
fn check() {
    rx.recv_timeout(Duration::from_secs(1)).expect("message must arrive");
}
RS
reject tests/recv_timeout.rs

cat >"$fixtures/tests/unpaused_tokio.rs" <<'RS'
#[tokio::test]
async fn check() {
    let s = tokio::time::Instant::now();
    assert!(s.elapsed() < d);
}
RS
reject tests/unpaused_tokio.rs

cat >"$fixtures/tests/alias_without_elapsed.rs" <<'RS'
use std::time::Instant as Clock;
#[test]
fn check() {
    let a = Clock::now();
    assert!(Clock::now() > a);
}
RS
reject tests/alias_without_elapsed.rs

cat >"$fixtures/attribute_only.rs" <<'RS'
#[test]
fn check() {
    let s = std::time::Instant::now();
    assert!(s.elapsed() < Duration::from_secs(1));
}
RS
reject attribute_only.rs

for method in le lt gt ge; do
  cat >"$fixtures/tests/method_$method.rs" <<RS
#[test]
fn check() {
    let start = Instant::now();
    let took = start.elapsed();
    let limit = Duration::from_secs(1);
    assert!(took.$method(&limit));
}
RS
  reject "tests/method_$method.rs"
done

for method in cmp partial_cmp; do
  if [ "$method" = cmp ]; then
    expected='Ordering::Less'
  else
    expected='Some(Ordering::Less)'
  fi
  cat >"$fixtures/tests/method_$method.rs" <<RS
#[test]
fn check() {
    let start = Instant::now();
    let took = start.elapsed();
    let limit = Duration::from_secs(1);
    assert_eq!(took.$method(&limit), $expected);
}
RS
  reject "tests/method_$method.rs"
done

for method in max min; do
  cat >"$fixtures/tests/method_$method.rs" <<RS
#[test]
fn check() {
    let start = Instant::now();
    let took = start.elapsed();
    assert!(took.$method(Duration::ZERO) < Duration::from_secs(1));
}
RS
  reject "tests/method_$method.rs"
done

cat >"$fixtures/direct.rs" <<'RS'
#[cfg(test)]
mod tests {
    #[test]
    fn direct() {
        let start = std::time::Instant::now();
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
RS
reject direct.rs

cat >"$fixtures/aliased.rs" <<'RS'
#[cfg(test)]
mod checks {
    use std::time::Instant as Clock;
    #[test]
    fn aliases() {
        let start = Clock::now();
        let measured = start
            .elapsed()
            .as_millis();
        let copied = measured;
        assert!(
            copied < 50
        );
    }
}
RS
reject aliased.rs

cat >"$fixtures/tests/block_let.rs" <<'RS'
#[test]
fn check() {
    let ticks = {
        let start = Instant::now();
        operation();
        start.elapsed()
    };
    assert!(ticks < Duration::from_secs(1));
}
RS
reject tests/block_let.rs

cat >"$fixtures/tests/block_on.rs" <<'RS'
#[test]
fn check() {
    let ticks = rt.block_on(async {
        let start = Instant::now();
        operation().await;
        start.elapsed()
    });
    assert!(ticks < Duration::from_secs(1));
}
RS
reject tests/block_on.rs

cat >"$fixtures/tests/reassignment.rs" <<'RS'
#[test]
fn check() {
    let mut ticks = Duration::ZERO;
    let start = Instant::now();
    ticks = start.elapsed();
    assert!(ticks < Duration::from_secs(1));
}
RS
reject tests/reassignment.rs

cat >"$fixtures/tests/tuple.rs" <<'RS'
#[test]
fn check() {
    let start = Instant::now();
    let (answer, ticks) = (operation(), start.elapsed());
    assert!(ticks < Duration::from_secs(1));
}
RS
reject tests/tuple.rs

cat >"$fixtures/tests/joined_clock.rs" <<'RS'
#[test]
fn check() {
    let worker = std::thread::spawn(|| {
        let start = Instant::now();
        operation();
        start.elapsed()
    });
    let ticks = worker.join().unwrap();
    assert!(ticks < Duration::from_secs(1));
}
RS
reject tests/joined_clock.rs

cat >"$fixtures/clock_containers.rs" <<'RS'
#[test]
fn outcome_not_elapsed() {
    let worker = std::thread::spawn(|| {
        let deadline = Instant::now();
        read_bytes()
    });
    let bytes = worker.join().unwrap();
    assert!(bytes > 0);
    let credentials = Credentials {
        expires: Some(SystemTime::now()),
    };
    assert_eq!(credentials.access_key_id(), "key");
}
struct Cache {
    #[cfg(test)]
    armed: Option<Instant>,
}
impl Cache {
    fn new() -> Self {
        Self {
            #[cfg(test)]
            armed: None,
        }
    }
    fn production() {
        let start = Instant::now();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
RS
bash "$checker" "$fixtures/clock_containers.rs"

cat >"$fixtures/tests/deadline.rs" <<'RS'
fn wait() {
    let deadline = Instant::now() + Duration::from_secs(1);
    if Instant::now() >= deadline { panic!("late"); }
}
RS
reject tests/deadline.rs

cat >"$fixtures/tests/equality.rs" <<'RS'
fn check() {
    let elapsed = timer.elapsed();
    assert_eq!(elapsed.as_secs(), 2);
}
RS
reject tests/equality.rs

cat >"$fixtures/tests/destructured.rs" <<'RS'
fn check() {
    let Outcome { elapsed, .. } = operation();
    assert!(elapsed >= Duration::from_millis(10));
}
RS
reject tests/destructured.rs

cat >"$fixtures/tests/returned.rs" <<'RS'
fn check() {
    let measure = || {
        let start = Instant::now();
        operation();
        start.elapsed()
    };
    let large_elapsed = measure();
    assert!(large_elapsed < Duration::from_secs(2));
}
RS
reject tests/returned.rs

cat >"$fixtures/tests/timestamps.rs" <<'RS'
fn check() {
    let log = vec![Instant::now(), Instant::now()];
    let a_time = log[0];
    let b_time = log[1];
    assert!(a_time < b_time);
}
RS
reject tests/timestamps.rs

cat >"$fixtures/tests/timeout.rs" <<'RS'
async fn check() {
    tokio::time::timeout(Duration::from_millis(100), operation())
        .await.expect("must finish");
}
RS
reject tests/timeout.rs

cat >"$fixtures/tests/scope.rs" <<'RS'
fn checks() {
    // wall-clock-ok: bounded wait for a child process to exit.
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(start.elapsed() < Duration::from_secs(1));
}
RS
reject tests/scope.rs

cat >"$fixtures/tests/empty_marker.rs" <<'RS'
fn check() {
    // wall-clock-ok:
    assert!(start.elapsed() < Duration::from_secs(5));
}
RS
reject tests/empty_marker.rs

cat >"$fixtures/negative.rs" <<'RS'
// Production timing is outside this check, even after a test-only import.
#[cfg(test)]
use std::time::Instant;
fn production() {
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[cfg(test)]
mod checks {
    #[test]
    fn allowed() {
        let start = Instant::now();
        // wall-clock-ok: hang guard for a real subprocess handshake.
        assert!(start.elapsed() < Duration::from_secs(30));
        // wall-clock-ok: bounds a real socket accept loop, not execution speed.
        if Instant::now() >= deadline { panic!("hung"); }
    }
    #[tokio::test(start_paused = true)]
    async fn virtual_time() {
        let start = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let delta = start.elapsed();
        assert_eq!(delta, Duration::from_secs(2));
        tokio::time::timeout(Duration::from_secs(10), operation()).await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn virtual_blocks_and_tuples() {
        let ticks = {
            let start = tokio::time::Instant::now();
            tokio::time::sleep(Duration::from_secs(2)).await;
            start.elapsed()
        };
        assert_eq!(ticks, Duration::from_secs(2));
        let (one, two) = (1, 2);
        assert_eq!(one + two, 3);
    }
    #[test]
    fn reassigned_constant() {
        let start = Instant::now();
        let mut ticks = start.elapsed();
        ticks = Duration::ZERO;
        assert_eq!(ticks, Duration::ZERO);
    }
    #[test]
    fn strings_and_duration_configuration() {
        let text = r###"assert!(start.elapsed() < Duration::ZERO); }"###;
        let text = "Instant::now() < deadline";
        /* assert!(start.elapsed() < Duration::ZERO); */
        assert_eq!(configured_timeout, Duration::from_secs(30));
        let elapsed = Duration::from_secs(2);
        assert_eq!(elapsed, Duration::from_secs(2));
        assert!(elapsed.le(&Duration::from_secs(2)));
        assert!(elapsed.max(Duration::ZERO) <= Duration::from_secs(2));
    }
}
fn production_after_tests() {
    assert!(start.elapsed() < Duration::from_secs(1));
}
RS
bash "$checker" "$fixtures/negative.rs"

cat >"$fixtures/tests/std_with_paused.rs" <<'RS'
#[tokio::test(start_paused = true)]
async fn still_real() {
    use tokio::time::Instant;
    let start = std::time::Instant::now();
    assert!(start.elapsed() < Duration::from_secs(1));
}
RS
reject tests/std_with_paused.rs

# Additional clocks are real even on a paused Tokio runtime.
for clock in 'chrono::Utc' 'chrono::Local' 'std::time::SystemTime'; do
  cat >"$fixtures/tests/clock.rs" <<RS
#[tokio::test(start_paused = true)]
async fn check() {
    let now = $clock::now();
    assert!(now <= $clock::now());
}
RS
  reject tests/clock.rs
done

for method in wait_timeout wait_timeout_while; do
  cat >"$fixtures/tests/condvar.rs" <<RS
fn check() {
    let result = condvar.$method(guard, Duration::from_secs(1));
    assert!(result.is_ok());
}
RS
  reject tests/condvar.rs
done

cat >"$fixtures/tests/select.rs" <<'RS'
#[tokio::test]
async fn check() {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(1)) => {
            panic!("hung");
        }
        _ = operation() => {}
    }
}
RS
reject tests/select.rs

# Array subscripts are strings in awk: depth 10+ must use numeric ordering.
cat >"$fixtures/tests/deep_select.rs" <<'RS'
#[tokio::test]
async fn check() {
    { { { { { { { { { { { {
        tokio::select! {
            _ = sleep(Duration::from_secs(1)) => {
                panic!("hung at depth greater than ten");
            }
            _ = operation() => {}
        }
    } } } } } } } } } } } }
}
RS
reject tests/deep_select.rs

# Also cross from a single-digit arm depth into a two-digit panic depth.
cat >"$fixtures/tests/deep_sleep_arm.rs" <<'RS'
#[tokio::test]
async fn check() {
    tokio::select! {
        _ = sleep(Duration::from_secs(1)) => {
            { { { { { { { { { { { {
                panic!("hung inside deeply nested sleep arm");
            } } } } } } } } } } } }
        }
        _ = operation() => {}
    }
}
RS
reject tests/deep_sleep_arm.rs
cat >"$fixtures/tests/select_assert.rs" <<'RS'
#[tokio::test]
async fn check() {
    tokio::select! {
        _ = sleep(Duration::from_secs(1)) => assert!(false, "hung"),
        _ = operation() => {}
    }
}
RS
reject tests/select_assert.rs

cat >"$fixtures/tests/select_error.rs" <<'RS'
#[tokio::test]
async fn check() {
    let result = tokio::select! {
        _ = sleep(Duration::from_secs(1)) => Err("hung"),
        result = operation() => result,
    };
    result.unwrap();
}
RS
reject tests/select_error.rs

cat >"$fixtures/tests/paused_condvar.rs" <<'RS'
#[tokio::test(start_paused = true)]
async fn check() {
    Condvar::wait_timeout(&condition, guard, Duration::from_secs(1)).unwrap();
}
RS
reject tests/paused_condvar.rs

# A malformed marker is itself an error, including before harmless code.
for marker in '// wall-clock-ok:' '// wall-clock-ok:     ' '// wall-clock-ok'; do
  cat >"$fixtures/tests/invalid_marker.rs" <<RS
fn check() {
    $marker
    assert_eq!(2, 2);
}
RS
  reject tests/invalid_marker.rs
  if ! grep -q 'invalid wall-clock-ok marker' "$fixtures/output"; then
    cat "$fixtures/output" >&2
    exit 1
  fi
done

cat >"$fixtures/tests/virtual_select.rs" <<'RS'
#[tokio::test(start_paused = true)]
async fn check() {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(1)) => { panic!("hung"); }
        _ = operation() => {}
    }
}
#[tokio::test]
async fn no_failure_on_timer() {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        _ = operation() => { panic!("operation failed"); }
    }
}
#[tokio::test]
async fn documented_guard() {
    // wall-clock-ok: bounds an external process handshake.
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(1)) => { panic!("hung"); }
        _ = operation() => {}
    }
}
RS
bash "$checker" "$fixtures/tests/virtual_select.rs"

echo "OK: wall-clock assertion checker fixtures passed."
