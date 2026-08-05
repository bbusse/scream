BIN=target/release/scream
HASH   := $(shell git rev-parse --short HEAD)
REMOTE ?= gh
RELEASE_BRANCH ?= ci

all: build strip

build:
	cargo build --release

strip:
	strip $(BIN)

clean:
	cargo clean

_check-remote:
	@git remote get-url $(REMOTE) > /dev/null 2>&1 || \
	    { echo "Error: no remote '$(REMOTE)' — add one with: git remote add $(REMOTE) <url>"; exit 1; }

_check-branch:
	@current="$$(git rev-parse --abbrev-ref HEAD)"; \
	if [ "$$current" != "$(RELEASE_BRANCH)" ]; then \
	    echo "Error: on branch '$$current' — releases must be tagged from '$(RELEASE_BRANCH)'. Checkout $(RELEASE_BRANCH) first."; \
	    exit 1; \
	fi

_check-up-to-date: _check-remote _check-branch
	@git fetch $(REMOTE) $(RELEASE_BRANCH) > /dev/null 2>&1
	@git merge-base --is-ancestor $(REMOTE)/$(RELEASE_BRANCH) HEAD || \
	    { echo "Error: $(RELEASE_BRANCH) has commits you don't have — pull/rebase before tagging a release."; exit 1; }

release: _check-up-to-date
	$(eval TAG := release-$(HASH))
	git tag -f $(TAG)
	@printf 'Tagged %s as %s\n' "$(HASH)" "$(TAG)"
	@printf 'Push tag to trigger a release? [y/N] ' && read ans && \
	    case "$$ans" in [yY]) git push $(REMOTE) $(TAG) ;; \
	    *) git tag -d $(TAG); echo 'Aborted — tag removed.' ;; esac

release-candidate rc: _check-up-to-date
	$(eval TAG := rc-$(HASH))
	git tag -f $(TAG)
	@printf 'Tagged %s as %s\n' "$(HASH)" "$(TAG)"
	@printf 'Push tag to trigger a release candidate? [y/N] ' && read ans && \
	    case "$$ans" in [yY]) git push $(REMOTE) $(TAG) ;; \
	    *) git tag -d $(TAG); echo 'Aborted — tag removed.' ;; esac

.PHONY: all build strip clean release release-candidate rc _check-remote _check-branch _check-up-to-date
