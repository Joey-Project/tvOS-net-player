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

test-media-network:
    env -u CI -u LOCAL_BUILD_MODE python3 scripts/media-network-e2e.py

test-tvos:
    scripts/test-tvos-simulator.sh

test-ui-e2e-macos:
    bash scripts/test-ui-e2e.sh macos

test-ui-e2e-tvos:
    bash scripts/test-ui-e2e.sh tvos
