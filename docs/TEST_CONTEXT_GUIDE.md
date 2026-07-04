# Test Context Guide

`generate_test_context` is an MCP tool that analyzes your codebase to find untested symbols, rank them by priority, and return everything needed to write tests: source code, callers, callees, example tests from the repo, and framework-specific templates with conventions and scaffolds.

## Quick Start

```
# Via MCP (Claude Code, Cursor, etc.)
generate_test_context(path="/your/project", file="src/auth.py")

# With specific test type
generate_test_context(path="/your/project", file="src/auth.py", test_type="integration")
```

## Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `path` | yes | Project root path |
| `file` | no | Specific file to analyze. If omitted, analyzes entire project |
| `symbol` | no | Specific symbol to get test context for |
| `test_type` | no | Filter templates: `unit`, `integration`, `functional`, `e2e` |

## What It Returns

1. **Untested symbols** -- functions/methods with no `TESTED_BY` edges in the graph, ranked by caller count (more callers = higher priority)
2. **Source code** -- full source of each untested symbol
3. **Callers & callees** -- who calls this function, what it calls (for mocking decisions)
4. **Example tests** -- existing tests from the repo that test similar symbols (for style matching)
5. **Framework detection** -- auto-detects your test framework from dependencies
6. **Templates** -- conventions + scaffolds for your detected framework and requested test type

## Framework Detection

Infigraph auto-detects your test framework by scanning dependency files (`Cargo.toml`, `package.json`, `requirements.txt`, `pom.xml`, `go.mod`, `*.csproj`, `Gemfile`, `Package.swift`, `mix.exs`, `build.gradle`, `build.sbt`).

## Supported Frameworks (18)

| Framework | Language | Test Types |
|-----------|----------|------------|
| **Rust** (cargo test) | Rust | unit, integration, e2e |
| **pytest** | Python | unit, integration, functional, e2e |
| **unittest** | Python | unit, integration |
| **JUnit** | Java | unit, integration, e2e |
| **TestNG** | Java | unit, integration |
| **Jest / Vitest** | JavaScript/TypeScript | unit, integration, e2e |
| **Mocha** | JavaScript/TypeScript | unit, integration |
| **Playwright / Cypress** | JavaScript/TypeScript | e2e |
| **Karate** | Java/API | integration, e2e |
| **Go** (testing) | Go | unit, integration, e2e |
| **NUnit / xUnit / MSTest** | C# | unit, integration |
| **Kotlin** (JUnit5 / Kotest) | Kotlin | unit, integration |
| **ScalaTest** | Scala | unit, integration |
| **RSpec** | Ruby | unit, integration |
| **Minitest** | Ruby | unit, integration |
| **XCTest** | Swift | unit, integration |
| **ExUnit** | Elixir | unit, integration |
| **Cucumber / Gherkin** | Multi-language | functional, e2e |

## Test Types

### Unit

Tests a single function or method in isolation. Dependencies are mocked/stubbed.

- **When to use:** Testing pure logic, data transformations, validators, utility functions
- **Conventions:** Fast, no I/O, no network, no database. One assertion per concept.

### Integration

Tests multiple components working together with real dependencies.

- **When to use:** Testing database queries, API client calls, service interactions, middleware chains
- **Conventions:** May use real DB (test instance), real filesystem. Slower than unit. Setup/teardown for shared state.

### Functional

Tests complete feature workflows from the user's perspective.

- **When to use:** Testing API endpoints end-to-end, multi-step user flows, business process workflows
- **Conventions:** Uses test client/fixtures. Verifies side effects (DB state, events emitted). Often marked separately for CI.

### E2E (End-to-End)

Tests the full system as deployed, including UI, CLI, or external service interactions.

- **When to use:** Testing CLI commands, browser flows, deployed service health, cross-service interactions
- **Conventions:** Slowest. May require docker/server startup. Often run separately from CI.

## Framework Details

### Rust (cargo test)

**Unit tests** go in `mod tests` at the bottom of the source file. Use `#[test]` or `#[rstest]` for parametrized tests. Mock with `mockall`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_name_scenario() {
        let input = todo!();
        let result = function_name(input);
        assert_eq!(result, expected);
    }
}
```

**Integration tests** go in `tests/` directory. Each file is a separate crate. Use `#[tokio::test]` for async.

**E2E tests** use `assert_cmd` for CLI testing or spawn server + `reqwest` for HTTP.

### Python (pytest)

**Unit tests** use `@pytest.mark.parametrize` for data-driven tests. Fixtures for setup/teardown. Name files `test_*.py`.

```python
import pytest

def test_function_name_scenario():
    result = function_name(input_val)
    assert result == expected

@pytest.mark.parametrize("input_val,expected", [
    (case1_in, case1_out),
    (case2_in, case2_out),
])
def test_function_name_parametrized(input_val, expected):
    assert function_name(input_val) == expected
```

**Integration tests** use `scope="session"` or `"module"` fixtures. Mark with `@pytest.mark.integration`.

**Functional tests** use test client fixtures. Mark with `@pytest.mark.functional`.

**E2E tests** use subprocess or docker fixtures. Mark with `@pytest.mark.e2e`.

### Python (unittest)

**Unit tests** extend `unittest.TestCase`. Use `setUp`/`tearDown` for lifecycle. Assert with `self.assertEqual`.

**Integration tests** use `setUpClass` for expensive shared setup.

### Java (JUnit 5)

**Unit tests** use `@Test`, `@ParameterizedTest`, `@MockBean`. Assert with `assertEquals`/`assertThrows`.

```java
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

class FunctionNameTest {
    @Test
    void shouldReturnExpected_whenGivenInput() {
        var result = functionName(input);
        assertEquals(expected, result);
    }
}
```

**Integration tests** use `@SpringBootTest` or `@DataJpaTest`. Annotate with `@Tag("integration")`.

**E2E tests** use `@SpringBootTest(webEnvironment = RANDOM_PORT)` + `TestRestTemplate`.

### Java (TestNG)

Similar to JUnit but uses `@Test(groups = "unit")`, `@DataProvider` for parametrized, `@BeforeClass`/`@AfterClass`.

### JavaScript/TypeScript (Jest / Vitest)

**Unit tests** use `describe`/`it` blocks. Mock with `jest.mock()` or `vi.mock()`.

```javascript
describe('functionName', () => {
  it('should return expected when given input', () => {
    const result = functionName(input);
    expect(result).toBe(expected);
  });
});
```

**Integration tests** use real modules, DB fixtures. Group in `__tests__/integration/`.

**E2E tests** use Playwright or Cypress (see below).

### JavaScript/TypeScript (Mocha)

Uses `describe`/`it` with Chai assertions (`expect(x).to.equal(y)`). Hooks: `before`, `after`, `beforeEach`, `afterEach`.

### Playwright / Cypress (E2E)

```javascript
import { test, expect } from '@playwright/test';

test('scenario description', async ({ page }) => {
  await page.goto('/path');
  await page.fill('[name="field"]', 'value');
  await page.click('button[type="submit"]');
  await expect(page.locator('.result')).toContainText('expected');
});
```

### Karate (API Testing)

**Integration tests** use `Scenario:` blocks with `Given/When/Then` DSL for REST API testing.

```gherkin
Feature: API endpoint test

  Scenario: Verify endpoint returns expected
    Given url baseUrl + '/endpoint'
    And request { "key": "value" }
    When method post
    Then status 200
    And match response.field == 'expected'
```

**E2E tests** chain multiple API calls with shared state via `* def`.

### Go (testing)

**Unit tests** are in `_test.go` files. Use `t.Run` for subtests, `testify` for assertions.

```go
func TestFunctionName_Scenario(t *testing.T) {
    result := FunctionName(input)
    assert.Equal(t, expected, result)
}
```

**Integration tests** use `//go:build integration` build tag. Skip with `t.Skip` if deps unavailable.

**E2E tests** use `exec.Command` for CLI or `httptest.NewServer` for HTTP.

### C# (NUnit / xUnit / MSTest)

**Unit tests** use `[Test]` (NUnit), `[Fact]`/`[Theory]` (xUnit), or `[TestMethod]` (MSTest). Mock with Moq.

**Integration tests** use `WebApplicationFactory<T>` for ASP.NET or real DB with test containers.

### Kotlin (JUnit5 / Kotest)

**Unit tests** use `@Test` with `assertEquals` or Kotest's `shouldBe` DSL.

**Integration tests** use `@SpringBootTest` or `@Testcontainers`.

### Scala (ScalaTest)

**Unit tests** use `AnyFunSuite` or `AnyFlatSpec` with `should`/`in` matchers.

**Integration tests** use `BeforeAndAfterAll` for setup.

### Ruby (RSpec)

**Unit tests** use `describe`/`it` with `expect(x).to eq(y)`. Mock with `allow(obj).to receive(:method)`.

**Integration tests** use `before(:all)` for shared setup.

### Ruby (Minitest)

**Unit tests** extend `Minitest::Test`. Assert with `assert_equal`.

**Integration tests** use `setup`/`teardown` with real dependencies.

### Swift (XCTest)

**Unit tests** extend `XCTestCase`. Assert with `XCTAssertEqual`.

**Integration tests** use `setUpWithError` for async setup.

### Elixir (ExUnit)

**Unit tests** use `test "description" do...end`. Assert with `assert`/`refute`.

**Integration tests** use `setup` callbacks with shared state via context.

### Cucumber / Gherkin (BDD)

**Functional tests** use `.feature` files with `Given/When/Then` steps.

```gherkin
Feature: Feature name
  Scenario: Scenario description
    Given precondition
    When action
    Then expected outcome
```

**E2E tests** chain scenarios with `Background:` for shared setup.

## How Templates Work

1. **Framework detection**: Infigraph scans your dependency files to identify the test framework
2. **Template matching**: Based on detected framework + requested `test_type`, returns matching conventions and scaffold
3. **Placeholder substitution**: Scaffolds use `FUNCTION_NAME`, `SCENARIO`, `EXPECTED`, `TODO` as placeholders -- replace with actual values
4. **Style matching**: If existing tests are found in the repo, their style takes precedence over templates

## Integration with Hooks

When using Claude Code, infigraph hooks enforce that `generate_test_context` is called before writing any test file. This ensures:

- Tests target the right symbols (highest-priority untested code)
- Tests follow the project's existing test conventions
- Tests use the correct framework patterns
- No tests are written from memory without codebase context
