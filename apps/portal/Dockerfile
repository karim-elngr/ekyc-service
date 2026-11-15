FROM node:24-bookworm-slim
WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN npm install --global "$(node -p 'require("./package.json").packageManager')" && chown node:node /app
USER node
RUN pnpm install --frozen-lockfile
COPY --chown=node:node . .
RUN pnpm build
EXPOSE 5173
CMD ["pnpm", "preview", "--host", "0.0.0.0"]
