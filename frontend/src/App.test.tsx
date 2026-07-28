import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { App } from "./App";

describe("App", () => {
  it("shows the control panel and every verification level", () => {
    render(<App />);

    expect(screen.getByRole("heading", { name: "Stackhour Control Panel" })).toBeVisible();
    expect(screen.getAllByRole("listitem")).toHaveLength(3);
    expect(screen.getByText(/coverage, build, dependency graph/)).toBeVisible();
  });
});
