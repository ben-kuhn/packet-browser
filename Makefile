NIX = nix --extra-experimental-features "nix-command flakes"

.PHONY: test build smoke-test e2e all install-hooks

## Run cargo unit tests
test:
	$(NIX) develop -c cargo test --all-features -- --test-threads=1

## Run e2e integration tests (requires Direwolf, PipeWire, LinBPQ)
e2e:
	@echo "=== Running e2e integration tests ==="
	@cd e2e && pip install -q -r requirements-test.txt && pytest -v

## Build the Docker image via Nix
build:
	$(NIX) build .#docker-image
	docker load < result

## Run smoke test against the already-loaded packet-browser:latest image
smoke-test:
	@echo "=== Smoke test: verifying Firefox starts and can load a page ==="
	@mkdir -p /tmp/smoke-logs

	@docker run -d --name smoke-test \
	  --read-only \
	  --tmpfs /tmp:size=512M,mode=1777 \
	  --tmpfs /dev/shm:size=128M,mode=1777 \
	  -p 127.0.0.1:63004:63004 \
	  -v /tmp/smoke-logs:/var/log/packet-browser \
	  --cap-drop ALL \
	  --security-opt seccomp=./packaging/seccomp/firefox.json \
	  --security-opt no-new-privileges \
	  -e BLOCKLIST_ENABLED=false \
	  -e PORTAL_URL=https://example.com \
	  packet-browser:latest

	@echo "Waiting for packet-browser to start..."
	@timeout 30 bash -c 'until nc -z 127.0.0.1 63004 2>/dev/null; do sleep 1; done' \
	  || (docker logs smoke-test; docker rm -f smoke-test; echo "FAIL: service did not start"; exit 1)

	@{ sleep 1; echo "W1TEST"; sleep 1; echo "AGREE"; sleep 180; } \
	  | nc 127.0.0.1 63004 >/dev/null 2>&1 & echo $$! > /tmp/smoke-nc.pid

	@echo "Step 1/2: Waiting for WebDriver session ready (up to 60s)..."
	@CONNECTED=0; \
	for i in $$(seq 1 60); do \
	  if docker logs smoke-test 2>&1 | grep -q '\[BROWSER\] Session ready'; then \
	    echo "  Firefox connected after $${i}s"; CONNECTED=1; break; \
	  fi; \
	  sleep 1; \
	done; \
	if [ $$CONNECTED -eq 0 ]; then \
	  echo "--- Container logs ---"; docker logs smoke-test 2>&1; \
	  kill $$(cat /tmp/smoke-nc.pid) 2>/dev/null || true; \
	  docker stop smoke-test 2>/dev/null || true; docker rm smoke-test 2>/dev/null || true; \
	  echo "FAIL: Firefox did not connect"; exit 1; \
	fi; \
	echo "Step 2/2: Waiting for a fully rendered + compressed page (up to 60s)..."; \
	RESULT=1; \
	for i in $$(seq 1 60); do \
	  if docker logs smoke-test 2>&1 | grep -q '\[SEND\]'; then \
	    echo "PASS: page rendered and compressed after $${i}s"; RESULT=0; break; \
	  fi; \
	  sleep 1; \
	done; \
	echo "--- Container logs ---"; \
	docker logs smoke-test 2>&1; \
	kill $$(cat /tmp/smoke-nc.pid) 2>/dev/null || true; \
	docker stop smoke-test 2>/dev/null || true; \
	docker rm smoke-test 2>/dev/null || true; \
	exit $$RESULT

## Build image then run smoke test
test-image: build smoke-test

## Run all checks (unit tests + build + smoke test)
all: test test-image

## Install git pre-push hook to run smoke test before every push
install-hooks:
	@cp scripts/pre-push .git/hooks/pre-push
	@chmod +x .git/hooks/pre-push
	@echo "Pre-push hook installed. 'make build' before pushing to populate packet-browser:latest."
