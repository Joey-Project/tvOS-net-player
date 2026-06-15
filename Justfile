set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    @just --list

build:
    scripts/build.sh

build-macos:
    scripts/build-macos.sh

build-cache-server:
    scripts/build-cache-server.sh

build-for-testing:
    scripts/build-for-testing.sh

ci: lint build build-macos build-cache-server build-for-testing test-tvos test test-macos test-cache-server

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

test-macos:
    scripts/test-macos.sh

test-tvos:
    scripts/test-tvos-simulator.sh
