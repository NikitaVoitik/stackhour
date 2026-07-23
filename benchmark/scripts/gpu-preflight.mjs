import { gpuEnvironment, verifyGpu } from "./gpu-harness.mjs";

const gpu = verifyGpu(gpuEnvironment());
console.log(JSON.stringify({ event: "gpu_preflight", ...gpu }, null, 2));
