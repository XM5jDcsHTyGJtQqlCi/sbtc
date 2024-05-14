install:
	pnpm install

build: install | emily-client
	cargo build \
		&& pnpm --recursive build

test: install | emily-client
	cargo test \
		&& pnpm --recursive test

integration-test: install | emily-client
	docker compose --file docker-compose.test.yml up --detach
	cargo test --all-features \
		&& pnpm --recursive test
	docker compose --file docker-compose.test.yml down

lint: install | emily-client
	cargo clippy -- -D warnings \
		&& pnpm --recursive run lint

clean:
	cargo clean \
		&& pnpm --recursive clean

# Emily API
# ----------------------------------------------------

EMILY_PATH=emily
EMILY_API_PROJECT_NAME=emily-api
EMILY_CDK_PROJECT_NAME=emily-cdk

emily-cdk: emily-client | emily-lambda
	pnpm --filter $(EMILY_CDK_PROJECT_NAME) run build

# Overwrite the default architecture with --arm64 configuration
# on arm machines.
ifneq (filter arm64 aarch64, $(shell uname -m),)
_LAMBDA_FLAGS := --arm64
endif
emily-lambda: emily-client
	cd emily/lambda \
		&& cargo lambda build \
			--release \
			--package emily-lambda \
			--output-format zip \
			$(_LAMBDA_FLAGS)

emily-client:
	pnpm --filter $(EMILY_API_PROJECT_NAME) run build
