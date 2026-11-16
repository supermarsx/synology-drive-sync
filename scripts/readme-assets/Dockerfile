# Image for the README screenshots: headless Chromium plus Vue 2.7.16 (the version the DSM
# AppWindow bundle is built against) and puppeteer-core. It only ever runs inside Docker, never
# on a desktop, and only ever loads the local harness page.
FROM node:26.13.1-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium fonts-liberation fonts-dejavu-core ca-certificates optipng \
 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /opt/pp && cd /opt/pp && npm init -y >/dev/null \
 && npm install --no-audit --no-fund puppeteer-core@25 vue@2.7.16
ENV NODE_PATH=/opt/pp/node_modules CHROME_BIN=/usr/bin/chromium
