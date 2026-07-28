const verificationLevels = [
  "Fast: format and type safety",
  "Standard: lint and unit tests",
  "Full: coverage, build, dependency graph, dead code, size, and audit",
] as const;

export function App(): React.JSX.Element {
  return (
    <main>
      <h1>Stackhour Control Panel</h1>
      <p>The frontend shell is ready for the control-plane client.</p>
      <ul aria-label="Verification levels">
        {verificationLevels.map((level) => (
          <li key={level}>{level}</li>
        ))}
      </ul>
    </main>
  );
}
