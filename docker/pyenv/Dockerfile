# The Python dev/verify sandbox (spec 12 — reproducible build environment).
#
# Generated apps + their frozen tests run INSIDE this container, never on the host:
#   docker run --rm -v <workspace>:/workspace -w /workspace dumb-coder-pyenv <cmd>
# so a missing package or a host-vs-container layout mismatch can't break a build, and
# the workspace is the only thing mounted in. `PYTHONPATH=/workspace` means a test's
# `from app import app` resolves against the project root regardless of where the file
# sits — fixing the import-roulette the host runs hit.
FROM python:3.12-slim

# Install the pinned toolkit once, into the image.
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

# Generated code is imported/run from the mounted workspace root.
ENV PYTHONPATH=/workspace
WORKDIR /workspace

# No default app — the harness passes the verify command (e.g. `pytest -q`) per run.
CMD ["python", "--version"]
