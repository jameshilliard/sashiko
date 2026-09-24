#!/usr/bin/env bash
# Copyright 2026 The Sashiko Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Helper script to launch a local Gemini HTTP-to-RPC proxy daemon when running
# in workstation environments with ambient credentials.

set -euo pipefail

find_proxy_binary() {
    if [ -n "${GEMINI_PROXY_BIN:-}" ] && [ -x "${GEMINI_PROXY_BIN}" ]; then
        echo "${GEMINI_PROXY_BIN}"
        return 0
    fi
    if command -v gemini_api_proxy >/dev/null 2>&1; then
        command -v gemini_api_proxy
        return 0
    fi
    local default_proxy="/google/bin/releases/gemini-cli/tools/gemini_api_proxy"
    if [ -x "${default_proxy}" ]; then
        echo "${default_proxy}"
        return 0
    fi
    return 1
}

if [ "${1:-}" = "--check" ]; then
    if find_proxy_binary >/dev/null; then
        exit 0
    else
        exit 1
    fi
fi

PORT_FILE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --port-file)
            PORT_FILE="$2"
            shift 2
            ;;
        --port-file=*)
            PORT_FILE="${1#*=}"
            shift
            ;;
        *)
            shift
            ;;
    esac
done

if [ -z "${PORT_FILE}" ]; then
    echo "Usage: $0 [--check | --port-file <path>]" >&2
    exit 1
fi

PROXY_BIN="$(find_proxy_binary)" || {
    echo "Error: No local gemini_api_proxy binary found." >&2
    exit 1
}

BACKEND="${GEMINI_PROXY_BACKEND:-blade:beyond-generative-service-prod-common}"
QUOTA_BUCKET="${GEMINI_PROXY_QUOTA_BUCKET:-developer-transform/shared-g3-gemini-quota}"
TRAFFIC_STREAM="${GEMINI_PROXY_TRAFFIC_STREAM:-gemini_cli}"
CRITICALITY="${GEMINI_PROXY_CRITICALITY:-CRITICAL_PLUS}"
USER_AGENT="${GEMINI_PROXY_USER_AGENT:-GeminiCLI/google3}"
PARENT_PID="${PPID}"

"${PROXY_BIN}" \
    --port=0 \
    --port_file="${PORT_FILE}" \
    --enable_sawmill_logging=false \
    --enable_sherlog_tracing=false \
    --user_agent="${USER_AGENT}" \
    --traffic_stream_selfid="${TRAFFIC_STREAM}" \
    --credential_exchanger_call_gaia_client_with_compass_stub_task_percentage=0 \
    --credential_exchanger_backend_deadline=30 \
    --envelope_enabled=false \
    --genai_backend="${BACKEND}" \
    --beyond_quota_bucket_key="${QUOTA_BUCKET}" \
    --default_criticality="${CRITICALITY}" &
PROXY_PID=$!

# Independent watchdog subshell ensures the proxy daemon is reaped even if the
# parent process or wrapper script receives SIGKILL.
(
    while kill -0 "${PROXY_PID}" 2>/dev/null; do
        if ! kill -0 "${PARENT_PID}" 2>/dev/null; then
            kill -TERM "${PROXY_PID}" 2>/dev/null || true
            sleep 1
            kill -KILL "${PROXY_PID}" 2>/dev/null || true
            rm -f "${PORT_FILE}"
            exit 0
        fi
        sleep 1
    done
) >/dev/null 2>&1 &
WATCHDOG_PID=$!

cleanup() {
    kill -TERM "${PROXY_PID}" "${WATCHDOG_PID}" 2>/dev/null || true
    rm -f "${PORT_FILE}"
}
trap cleanup EXIT TERM INT HUP

wait "${PROXY_PID}" 2>/dev/null || true
