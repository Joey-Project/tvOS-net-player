.PHONY: build build-for-testing deploy format install-hooks lint test test-tvos

build:
	scripts/build.sh

build-for-testing:
	scripts/build-for-testing.sh

format:
	scripts/format.sh

install-hooks:
	scripts/install-hooks.sh

test:
	scripts/test.sh

test-tvos:
	scripts/test-tvos-simulator.sh

lint:
	scripts/lint.sh

deploy:
	scripts/deploy-lan.sh
