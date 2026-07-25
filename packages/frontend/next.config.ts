import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // Emit a self-contained server (.next/standalone/server.js + a minimal
  // node_modules) so the Cloud Run image ships only what runtime needs — no
  // bun, no dev deps. See packages/frontend/Dockerfile.
  output: "standalone",
};

export default nextConfig;
