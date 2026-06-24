# The dev/verify sandbox (spec 12 — reproducible build environment).
#
# Generated apps + their frozen tests run INSIDE this container, never on the host:
#   docker run --rm -v <workspace>:/workspace -w /workspace dumb-coder-devenv <cmd>
# so a missing package or a host-vs-container layout mismatch can't break a build, and
# the workspace is the only thing mounted in. `PYTHONPATH=/workspace` means a test's
# `from app import app` resolves against the project root regardless of where the file
# sits.
#
# Two runners live here so each subtask is verified in its own language (spec 08):
# pytest for the Python backend, vitest for the plain-JS frontend.
FROM python:3.12-slim

# --- Node (for the frontend's vitest runner) ---
# nodejs from Debian is fine for running vitest headlessly; no build step needed.
RUN apt-get update \
    && apt-get install -y --no-install-recommends nodejs npm \
    && rm -rf /var/lib/apt/lists/*

# --- Python toolkit (pinned) ---
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

# --- JS test runner (vitest + jsdom for DOM tests), installed globally so a generated
# project needs no package.json/npm install of its own. ---
RUN npm install -g vitest@2 jsdom@25 \
    && npm cache clean --force

# Generated code is imported/run from the mounted workspace root.
ENV PYTHONPATH=/workspace
# Let a bare `import`/`require` in a test resolve the globally-installed vitest/jsdom.
ENV NODE_PATH=/usr/lib/node_modules
WORKDIR /workspace

CMD ["python", "--version"]
