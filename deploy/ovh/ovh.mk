# The shared OVH host's targets, from gcloud-ovh-migrate's service template: word for word the
# same in every service, so `make service-drift` there can say when this copy has moved.
# Included from the service's own Makefile:
#
#   include deploy/ovh/ovh.mk
#
# The host's address is written once, in gcloud-ovh-migrate's .env, and read from there.
OVH_PLATFORM ?= $(HOME)/Developer/gcloud-ovh-migrate
-include $(OVH_PLATFORM)/.env
export OVH_HOST OVH_SSH_USER OVH_PLATFORM

OVH_SERVICE := $(shell sed -n 's/^SERVICE=//p' deploy/ovh/service.env)
OVH_SECRETS := deploy/ovh/$(OVH_SERVICE).enc.env

.PHONY: ovh-install ovh-secrets-set ovh-secrets-show ovh-secrets-edit ovh-secrets-check ovh-stack-test ovh-logs

ovh-install: ## Copy the compose file, the secrets, the edge snippets and the timers to the OVH host
	./scripts/ovh-install.sh

ovh-secrets-set: ## Put one secret into the service's encrypted file from a hidden prompt: KEY=NAME
	./scripts/secrets.sh set $(OVH_SECRETS) "$(KEY)"

ovh-secrets-show: ## List the names the encrypted file holds, never their values
	./scripts/secrets.sh show $(OVH_SECRETS)

ovh-secrets-edit: ## Edit the encrypted file in sops, in a container
	./scripts/secrets.sh edit $(OVH_SECRETS)

ovh-secrets-check: ## Fail on anything secret about to be committed, or any value in clear
	./scripts/secrets-check.sh

ovh-stack-test: ## Run the stack here behind the real edge configuration and check it serves
	./scripts/ovh-stack-test.sh

# Read through Grafana by gcloud-ovh-migrate's own script, with the read-only token
# `make logs-token` stored there: nothing of it is copied here.
ovh-logs: ## This service's production logs: [SINCE=1h] [LEVEL=error] [COMPONENT=web] [GREP=text] [REQUEST=id] [LIMIT=200] [FORMAT=raw]
	@"$(OVH_PLATFORM)/scripts/ovh-logs.sh" "$(OVH_SERVICE)"
