set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    @just --list

build:
    scripts/build.sh

build-macos:
    scripts/build-macos.sh

build-cache-server:
    scripts/build-cache-server.sh

local-build *targets:
    python3 scripts/local_build.py build {{targets}}

local-build-status:
    python3 scripts/local_build.py status

build-for-testing:
    scripts/build-for-testing.sh

ci: lint test-ci-helpers build build-macos build-cache-server build-for-testing test-tvos test test-macos test-lan-playback test-cache-server

deploy:
    scripts/deploy-lan.sh

format:
    scripts/format.sh

install-hooks:
    scripts/install-hooks.sh

lint:
    scripts/lint.sh

pre-commit:
    scripts/pre-commit.sh

test:
    scripts/test.sh

test-cache-server:
    scripts/test-cache-server.sh

test-ci-helpers:
    python3 -m unittest discover -s scripts -p 'test_*.py' -v

test-bilibili-live:
    scripts/test-bilibili-live.sh

test-macos:
    scripts/test-macos.sh

test-lan-playback:
    bash scripts/test-lan-playback.sh

test-tvos:
    scripts/test-tvos-simulator.sh

test-ui-e2e-macos:
    if [[ "${CI:-}" == "true" || "${LOCAL_BUILD_MODE:-}" == "isolated" ]]; then if [[ -z "${CACHE_SERVER_BINARY:-}" ]]; then scripts/build-cache-server.sh; fi; python3 scripts/ui-e2e.py --platform macos; else CACHE_SERVER_BINARY="$(python3 scripts/local_build.py prepare server --print-path server)" python3 scripts/ui-e2e.py --platform macos; fi

test-ui-e2e-tvos:
    if [[ "${CI:-}" == "true" || "${LOCAL_BUILD_MODE:-}" == "isolated" ]]; then if [[ -z "${CACHE_SERVER_BINARY:-}" ]]; then scripts/build-cache-server.sh; fi; python3 scripts/ui-e2e.py --platform tvos; else CACHE_SERVER_BINARY="$(python3 scripts/local_build.py prepare server --print-path server)" python3 scripts/ui-e2e.py --platform tvos; fi
