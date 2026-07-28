"use strict";

/** @type {import("dependency-cruiser").IConfiguration} */
module.exports = {
  forbidden: [
    {
      name: "no-circular",
      severity: "error",
      from: {},
      to: { circular: true },
    },
    {
      name: "no-orphans",
      severity: "error",
      from: {
        orphan: true,
        pathNot: ["(^|/)main\\.tsx$", "\\.test\\.tsx?$", "test-setup\\.ts$"],
      },
      to: {},
    },
    {
      name: "no-deprecated-dependencies",
      severity: "error",
      from: {},
      to: { dependencyTypes: ["deprecated"] },
    },
    {
      name: "no-undeclared-dependencies",
      severity: "error",
      from: {},
      to: { dependencyTypes: ["npm-no-pkg", "npm-unknown"] },
    },
  ],
  options: {
    doNotFollow: { path: "node_modules" },
    exclude: { path: "node_modules" },
    tsConfig: { fileName: "tsconfig.json" },
    tsPreCompilationDeps: true,
  },
};
