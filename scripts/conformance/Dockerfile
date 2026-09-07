FROM node:22-bookworm-slim
WORKDIR /opt/contextforge-integration
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts && npm cache clean --force
