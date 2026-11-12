#!/usr/bin/env bash
# Spin up a **disposable** relay on a non-production port, mirroring the real configuration.
#
# Why this exists
# ---------------
# The production relay on :20900 serves live traffic. Testing a critical change against it means
# either restarting it (brief outage, and a broken build takes the gateway down) or not testing the
# change at all. Neither is acceptable, so critical changes are validated on a second relay.
#
# The test relay is a true mirror, not a toy:
#   * it reads a *copy* of the real `config.json`, `vault.enc` and `vault.key`, so it has the same
#     providers, accounts, combos and usable credentials — a fix that works here works in production;
#   * it gets its **own** state directory, so usage records, cooldowns and the compiled snapshot it
#     writes never touch the production ones;
#   * it binds loopback only, so it is not reachable from the network even though it holds real
#     credentials.
#
# What it deliberately does not do
# --------------------------------
# It never reads or writes the production state directory, and it never stops, restarts or signals the
# production relay. If the production relay is running, it stays running and untouched.
#
# Usage
#   scripts/test-relay.sh start     # copy config, start on :20905, wait for /healthz
#   scripts/test-relay.sh stop
#   scripts/test-relay.sh status
#   scripts/test-relay.sh logs      # tail the test relay's log
#
# Port: override with TEST_RELAY_PORT. Default 20905.
set -euo pipefail

PORT="${TEST_RELAY_PORT:-20905}"
ROOT="${TEST_RELAY_ROOT:-/tmp/mr-test-relay-$PORT}"
CONFIG_DIR="$ROOT/config"
STATE_DIR="$ROOT/state"
LOG="$ROOT/relay.log"
PIDFILE="$ROOT/relay.pid"
PROD_CONFIG_DIR="${HOME}/.config/manarouter"
BIN="${TEST_RELAY_BIN:-target/release/manarouter-relay}"

export PATH="$HOME/.cargo/bin:$PATH"

die() { echo "test-relay: $*" >&2; exit 1; }

# The production directory must never be the thing we start against: that would share the state
# directory and defeat the entire point.
case "$CONFIG_DIR" in
  "$PROD_CONFIG_DIR") die "refusing to run against the production config directory" ;;
esac

is_running() {
  [[ -f "$PIDFILE" ]] || return 1
  local pid; pid="$(cat "$PIDFILE" 2>/dev/null || true)"
  [[ -n "$pid" ]] || return 1
  kill -0 "$pid" 2>/dev/null
}

cmd_start() {
  [[ -x "$BIN" ]] || die "no relay binary at $BIN (run: cargo build --release)"

  # A stale instance would hold the port and confuse every later check.
  if is_running; then
    echo "already running (pid $(cat "$PIDFILE")) on :$PORT"
    return 0
  fi

  if ss -ltn 2>/dev/null | grep -q ":$PORT "; then
    die "port $PORT is already in use by something this script does not manage"
  fi

  mkdir -p "$CONFIG_DIR" "$STATE_DIR"

  # Copy the three files that make a configuration usable. `vault.key` is what decrypts `vault.enc`;
  # without it the copy would start and then fail every request with an unreadable secret, which
  # looks exactly like a router bug and wastes an hour.
  for f in config.json vault.enc vault.key; do
    [[ -f "$PROD_CONFIG_DIR/$f" ]] || die "missing $PROD_CONFIG_DIR/$f"
    cp -p "$PROD_CONFIG_DIR/$f" "$CONFIG_DIR/$f"
  done
  chmod 600 "$CONFIG_DIR/vault.key" "$CONFIG_DIR/vault.enc"

  echo "starting test relay on 127.0.0.1:$PORT"
  echo "  config: $CONFIG_DIR (copy)"
  echo "  state:  $STATE_DIR (fresh, isolated)"

  # Loopback only: this instance holds real credentials.
  MANAROUTER_CONFIG_DIR="$CONFIG_DIR" \
  MANAROUTER_STATE_DIR="$STATE_DIR" \
  RUST_LOG="${RUST_LOG:-info}" \
    nohup "$BIN" --host 127.0.0.1 --port "$PORT" >>"$LOG" 2>&1 &
  echo $! > "$PIDFILE"

  # Wait for health rather than sleeping a fixed time: a relay that dies on startup (bad config,
  # port race, missing key) must be reported as a failure, not discovered later.
  for _ in $(seq 1 60); do
    if ! is_running; then
      echo "--- last 40 log lines ---" >&2
      tail -40 "$LOG" >&2
      rm -f "$PIDFILE"
      die "relay exited during startup"
    fi
    if curl -fsS --max-time 1 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
      echo "up: http://127.0.0.1:$PORT  (pid $(cat "$PIDFILE"))"
      return 0
    fi
    sleep 0.5
  done

  tail -40 "$LOG" >&2
  die "relay did not become healthy on :$PORT"
}

cmd_stop() {
  if ! is_running; then
    echo "not running"
    rm -f "$PIDFILE"
    return 0
  fi
  local pid; pid="$(cat "$PIDFILE")"
  kill "$pid" 2>/dev/null || true
  for _ in $(seq 1 40); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.25
  done
  kill -9 "$pid" 2>/dev/null || true
  rm -f "$PIDFILE"
  echo "stopped (pid $pid)"
}

cmd_status() {
  if is_running; then
    echo "running: pid $(cat "$PIDFILE") on http://127.0.0.1:$PORT"
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/healthz" 2>/dev/null || echo "(healthz not answering)"
  else
    echo "not running (port $PORT)"
  fi
  echo "config: $CONFIG_DIR"
  echo "state:  $STATE_DIR"
  echo "log:    $LOG"
}

cmd_logs() { tail -n "${1:-80}" "$LOG" 2>/dev/null || echo "no log at $LOG"; }

case "${1:-}" in
  start)  cmd_start ;;
  stop)   cmd_stop ;;
  status) cmd_status ;;
  logs)   cmd_logs "${2:-80}" ;;
  *) echo "usage: $0 {start|stop|status|logs [n]}" >&2; exit 2 ;;
esac
