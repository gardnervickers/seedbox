#!/usr/bin/env bash
# Installed by the host module, outside the runner's writable checkout.
# The installed hook terminates its job worker when this validator refuses.
set -euo pipefail
if [[ ${GITHUB_REPOSITORY:-} != gardnervickers/seedbox ||
      ${GITHUB_WORKFLOW_REF:-} != gardnervickers/seedbox/.github/workflows/host-check.yml@refs/heads/main ||
      ${GITHUB_JOB:-} != host ]]; then
  echo 'host runner refused job: repository, workflow ref or job is not allowed' >&2
  exit 1
fi
case ${GITHUB_EVENT_NAME:-} in
  pull_request_target|issue_comment) ;;
  *) echo 'host runner refused job: event is not allowed' >&2; exit 1 ;;
esac
