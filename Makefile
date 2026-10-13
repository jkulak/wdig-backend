.DEFAULT_GOAL := help

# Every command runs in a container: there is no cargo and no psql on the host. cargo runs in
# the builder stage with the source mounted and its caches in volumes, on the stack's network.
COMPOSE := docker compose
BUILDER := wdig-backend-builder
RUN_BUILDER := docker run --rm -v "$(CURDIR):/app" -v wdig-backend-cargo-registry:/usr/local/cargo/registry -v wdig-backend-cargo-target:/app/target -w /app --network wdig-backend_default $(BUILDER)

.PHONY: help
help: ## List every target
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-22s %s\n", $$1, $$2}'

.PHONY: up
up: ## Start the database and the service (build first when needed)
	$(COMPOSE) up -d --build --wait

.PHONY: down
down: ## Stop the stack
	$(COMPOSE) down

.PHONY: logs
logs: ## Follow the stack's logs
	$(COMPOSE) logs -f

.PHONY: build
build: ## Build the runtime image as a release does
	$(COMPOSE) -f compose.yaml build app

.PHONY: builder
builder: ## Build the toolchain image cargo runs in (once, and after a toolchain change)
	docker build --target toolchain -t $(BUILDER) .

.PHONY: test
test: builder ## Run cargo test in the builder image against the stack's database (a wdig_test database it creates and drops)
	./scripts/test.sh

.PHONY: lint
lint: builder ## cargo clippy, warnings as errors, in the builder image
	$(RUN_BUILDER) cargo clippy --all-targets --all-features -- -D warnings

.PHONY: cs
cs: builder ## cargo fmt --check, in the builder image
	$(RUN_BUILDER) cargo fmt --all -- --check

.PHONY: cs-fix
cs-fix: builder ## cargo fmt, in the builder image
	$(RUN_BUILDER) cargo fmt --all

.PHONY: migrate
migrate: ## Apply the pending migrations to the local database
	./scripts/migrate.sh up

.PHONY: migrate-down
migrate-down: ## Revert the last applied migration on the local database; N=2 for the last two, N=all for every one
	./scripts/migrate.sh down $(or $(N),1)

.PHONY: migrate-status
migrate-status: ## Show which migrations the local database has applied and which are pending
	./scripts/migrate.sh status

.PHONY: migrate-new
migrate-new: ## Create the next migration's up and down files: make migrate-new NAME=add-steps
	./scripts/migrate.sh new "$(NAME)"

.PHONY: reset-db
reset-db: ## Drop the local wdig database and build it again from the migrations (local stack only)
	./scripts/reset-db.sh

.PHONY: partitions
partitions: ## Create the next months' partitions of points, as the service does once a day
	$(COMPOSE) exec app wdig partitions

.PHONY: seed
seed: ## Write a made-up month over Warsaw's streets for an account: make seed EMAIL=kuba@example.com [DAYS=30] [TZ=Europe/Warsaw] [SEED=1]
	$(COMPOSE) exec app wdig seed --email "$(EMAIL)" --days $(or $(DAYS),30) --tz $(or $(TZ),Europe/Warsaw) --seed $(or $(SEED),1) --replace

.PHONY: invite
invite: ## Make invite codes for registration and print them: make invite [N=5]
	$(COMPOSE) exec app wdig invite --count $(or $(N),1)

.PHONY: smoke
smoke: ## Ask the local stack for its health, as the release's smoke test asks production
	SMOKE_URL=http://localhost:8090 ./deploy/ovh/smoke.sh

include deploy/ovh/ovh.mk
