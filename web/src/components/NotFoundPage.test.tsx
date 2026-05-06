import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { NotFoundPage } from "./NotFoundPage.js";

afterEach(cleanup);

describe("NotFoundPage", () => {
  it("renders 404 copy with the failed pathname and a back-to-dashboard link", () => {
    render(
      <MemoryRouter initialEntries={["/totally/unknown/path"]}>
        <Routes>
          <Route path="*" element={<NotFoundPage />} />
        </Routes>
      </MemoryRouter>,
    );

    expect(screen.getByText("404")).toBeTruthy();
    expect(screen.getByText(/Page not found/i)).toBeTruthy();
    expect(screen.getByText("/totally/unknown/path")).toBeTruthy();

    const back = screen.getByRole("link", { name: /Back to dashboard/i });
    expect((back as HTMLAnchorElement).getAttribute("href")).toBe("/");
  });
});
