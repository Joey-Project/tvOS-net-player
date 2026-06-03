set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    @just --list

build:
    scripts/build.sh

build-for-testing:
    scripts/build-for-testing.sh

ci: lint build build-for-testing test-tvos test

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

test-tvos:
    scripts/test-tvos-simulator.sh
