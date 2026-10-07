import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ControlPlaneClient } from "../src/client.js";
import {
  NotFoundError,
  PolicyViolationError,
  UnauthorizedError,
  UnsupportedControlPlaneCapabilityError,
  ValidationError,
} from "../src/errors.js";

describe("ControlPlaneClient", () => {
  const baseUrl = "https://control-plane.example.com";
  let client: ControlPlaneClient;
  let mockFetch: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    mockFetch = vi.fn();
    client = new ControlPlaneClient({
      baseUrl,
      token: "test-token-123",
      fetch: mockFetch as unknown as typeof fetch,
    });
  });

  it("attaches credentials to a route that Axum actually exposes", async () => {
    mockFetch.mockResolvedValueOnce(ok([]));
    await client.listOrganizations();
    expect(mockFetch).toHaveBeenCalledWith(
      `${baseUrl}/v1/organizations`,
      expect.objectContaining({
        headers: expect.objectContaining({
          Accept: "application/json",
          Authorization: "Bearer test-token-123",
        }),
      }),
    );

    const apiKeyClient = new ControlPlaneClient({
      baseUrl,
      apiKey: "cody_test_key_xyz",
      fetch: mockFetch as unknown as typeof fetch,
    });
    mockFetch.mockResolvedValueOnce(ok([]));
    await apiKeyClient.listOrganizations();
    expect(mockFetch.mock.calls[1]?.[1].headers["X-API-Key"]).toBe("cody_test_key_xyz");
  });

  it("forwards only caller-supplied idempotency keys and uses the deployed create body", async () => {
    mockFetch.mockResolvedValue(ok(organizationWire()));
    await client.createOrganization(
      { slug: "acme", displayName: "Acme Corp" },
      { idempotencyKey: "caller-key" },
    );
    const init = mockFetch.mock.calls[0]?.[1];
    expect(init.headers["Idempotency-Key"]).toBe("caller-key");
    expect(JSON.parse(init.body)).toEqual({
      slug: "acme",
      display_name: "Acme Corp",
      max_publication_class: null,
      max_classification: null,
    });

    await client.createOrganization({ slug: "beta", displayName: "Beta" });
    expect(mockFetch.mock.calls[1]?.[1].headers["Idempotency-Key"]).toBeUndefined();
  });

  it("refuses create settings the deployed route would silently ignore", async () => {
    await expect(
      client.createOrganization({
        slug: "acme",
        displayName: "Acme",
        retentionDays: 30,
      }),
    ).rejects.toThrow(UnsupportedControlPlaneCapabilityError);
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("normalizes server errors on implemented routes", async () => {
    mockFetch.mockResolvedValueOnce(failed(404, {
      type: "not_found",
      resource: "repository",
      message: "no such repository",
    }));
    await expect(client.getRepository("org-1", "repo-1")).rejects.toThrow(NotFoundError);

    mockFetch.mockResolvedValueOnce(failed(401, {
      type: "unauthorized",
      message: "no valid credential presented",
    }));
    await expect(client.listOrganizations()).rejects.toThrow(UnauthorizedError);

    mockFetch.mockResolvedValueOnce(failed(400, {
      type: "validation_error",
      message: "invalid request parameters",
    }));
    await expect(client.createOrganization({ slug: "", displayName: "test" })).rejects.toThrow(
      ValidationError,
    );

    mockFetch.mockResolvedValueOnce(failed(422, {
      type: "policy_violation",
      message: "ceiling exceeded",
    }));
    await expect(
      client.registerRepository("org-1", {
        federatedId: "a".repeat(64),
        displayName: "Repo",
      }),
    ).rejects.toThrow(PolicyViolationError);
  });

  it("adapts the generated refresh-token contract exactly", async () => {
    mockFetch.mockResolvedValueOnce(ok({
      access_token: "access-next",
      refresh_token: "refresh-next",
      expires_in: 3600,
      token_type: "Bearer",
      user: null,
    }));
    await expect(client.refreshToken({ refreshToken: "refresh-old" })).resolves.toEqual({
      accessToken: "access-next",
      refreshToken: "refresh-next",
      expiresIn: 3600,
      tokenType: "Bearer",
    });
    expect(client.getToken()).toBe("access-next");
    expect(JSON.parse(mockFetch.mock.calls[0]?.[1].body)).toEqual({
      refresh_token: "refresh-old",
    });
  });

  it("uses the generated pairing-start request and response", async () => {
    mockFetch.mockResolvedValueOnce(ok({
      challenge_code: "cp_pair_123",
      expires_at: "2026-08-17T11:00:00Z",
      verification_uri: "/pair?code=cp_pair_123",
      poll_interval_seconds: 5,
    }));
    const challenge = await client.createPairingChallenge({
      organizationId: "org-1",
      maxPublicationClass: "content-shared",
      acceptsRemoteApprovals: true,
    });
    expect(challenge.code).toBe("cp_pair_123");
    expect(JSON.parse(mockFetch.mock.calls[0]?.[1].body)).toEqual({
      organization_id: "org-1",
      requested_scope: {
        max_publication_class: "content-shared",
        accepts_remote_approvals: true,
        accepts_runner_dispatch: false,
      },
    });
  });

  it("adapts the deployed organization row without inventing updated_at", async () => {
    mockFetch.mockResolvedValueOnce(ok([organizationWire()]));
    const organizations = await client.listOrganizations();
    expect(organizations[0]).toMatchObject({
      id: "org-1",
      displayName: "Acme Corp",
      maxPublicationClass: "metadata-shared",
      createdAt: "2026-08-17T00:00:00Z",
    });
  });

  it("uses generated repository shapes in both directions", async () => {
    mockFetch.mockResolvedValueOnce(ok(repositoryWire()));
    const repository = await client.registerRepository("org-1", {
      federatedId: "a".repeat(64),
      displayName: "Repo",
      maxPublicationClass: "content-shared",
    });
    expect(repository.organizationId).toBe("org-1");
    expect(JSON.parse(mockFetch.mock.calls[0]?.[1].body)).toEqual({
      federated_id: "a".repeat(64),
      display_name: "Repo",
      max_publication_class: "content-shared",
      max_classification: null,
    });
  });

  it("requires repository scope and refuses filters Axum does not implement", async () => {
    await expect(client.listSharedSessions("org-1")).rejects.toThrow(ValidationError);
    await expect(
      client.listSharedSessions("org-1", { repositoryId: "repo-1", state: "running" }),
    ).rejects.toThrow(UnsupportedControlPlaneCapabilityError);
    expect(mockFetch).not.toHaveBeenCalled();

    mockFetch.mockResolvedValueOnce(ok([sessionWire()]));
    const page = await client.listSharedSessions("org-1", {
      repositoryId: "repo-1",
      limit: 10,
    });
    expect(page.items[0]?.remoteSessionKey).toBe("local-key");
    expect(mockFetch.mock.calls[0]?.[0]).toContain("repository_id=repo-1&limit=10");
  });

  it("sends only the audit query the deployed handler understands", async () => {
    await expect(
      client.listAuditRecords("org-1", { action: "repository.register" }),
    ).rejects.toThrow(UnsupportedControlPlaneCapabilityError);
    mockFetch.mockResolvedValueOnce(ok([]));
    await client.listAuditRecords("org-1", { limit: 20 });
    expect(mockFetch.mock.calls[0]?.[0]).toBe(
      `${baseUrl}/v1/organizations/org-1/audit?limit=20`,
    );
  });

  it("uses the actual organization member mutation", async () => {
    mockFetch.mockResolvedValueOnce(ok({ status: "added" }));
    await expect(
      client.addOrganizationMember("org-1", { userId: "user-2", role: "contributor" }),
    ).resolves.toEqual({ status: "added" });
    expect(JSON.parse(mockFetch.mock.calls[0]?.[1].body)).toEqual({
      user_id: "user-2",
      role: "contributor",
    });
  });

  it("uploads raw bytes and maps published-object metadata", async () => {
    mockFetch.mockResolvedValueOnce(ok(objectWire()));
    const object = await client.uploadObject(
      "org-1",
      { body: "hello", mediaType: "text/plain", expectedContentHash: "a".repeat(64) },
    );
    expect(object.contentHash).toBe("a".repeat(64));
    const init = mockFetch.mock.calls[0]?.[1];
    expect(init.body).toBe("hello");
    expect(init.headers["Content-Type"]).toBe("text/plain");
    expect(init.headers["X-Content-Sha256"]).toBe("a".repeat(64));
  });

  it("uses the actual presign and metadata routes", async () => {
    const hash = "a".repeat(64);
    mockFetch.mockResolvedValueOnce(ok({
      url: "https://objects.example/download",
      key: `org-1/${hash}`,
      method: "GET",
    }));
    await client.getPresignedDownloadUrl("org-1", hash);
    expect(mockFetch.mock.calls[0]?.[0]).toBe(`${baseUrl}/v1/organizations/org-1/objects/presign`);
    expect(JSON.parse(mockFetch.mock.calls[0]?.[1].body)).toEqual({ key: hash, method: "GET" });

    mockFetch.mockResolvedValueOnce(ok(objectWire()));
    await client.getObjectMetadata("org-1", hash);
    expect(mockFetch.mock.calls[1]?.[0]).toBe(
      `${baseUrl}/v1/organizations/org-1/objects/${hash}/metadata`,
    );
  });

  it("fails known-unimplemented capabilities locally instead of calling phantom routes", async () => {
    const operations = [
      () => client.getCurrentUser(),
      () => client.listTeams("org-1"),
      () => client.updateOrganizationPolicy("org-1", {}),
      () => client.getSharedSession("org-1", "session-1"),
      () => client.listPendingApprovals("org-1"),
      () => client.createApiKey("org-1", { organizationId: "org-1", name: "ci", role: "observer" }),
      () => client.listDaemons("org-1"),
    ];
    for (const operation of operations) {
      await expect(operation()).rejects.toThrow(UnsupportedControlPlaneCapabilityError);
    }
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("keeps every SDK network route anchored in the actual Axum router (0.8 `{param}` path syntax)", () => {
    const router = readFileSync(
      resolve(process.cwd(), "../../crates/control-plane/src/http.rs"),
      "utf8",
    );
    for (const route of [
      "/v1/auth/refresh",
      "/v1/auth/pairing/challenge",
      "/v1/organizations",
      "/v1/organizations/{id}/members",
      "/v1/organizations/{org_id}/repositories",
      "/v1/organizations/{org_id}/repositories/{id}",
      "/v1/organizations/{org_id}/sessions",
      "/v1/organizations/{org_id}/objects/upload",
      "/v1/organizations/{org_id}/objects/presign",
      "/v1/organizations/{org_id}/objects/{hash}/metadata",
      "/v1/organizations/{org_id}/audit",
    ]) {
      expect(router).toContain(`"${route}"`);
    }
  });
});

function ok(body: unknown) {
  return { ok: true, status: 200, json: async () => body };
}

function failed(status: number, body: unknown) {
  return { ok: false, status, statusText: "failed", json: async () => body };
}

function organizationWire() {
  return {
    id: "org-1",
    slug: "acme",
    display_name: "Acme Corp",
    max_publication_class: "metadata-shared",
    max_classification: "internal",
    policy_version: 1,
    created_at: "2026-08-17T00:00:00Z",
    data_residency: null,
    retention_days: null,
  };
}

function repositoryWire() {
  return {
    id: "repo-1",
    organization_id: "org-1",
    federated_id: "a".repeat(64),
    display_name: "Repo",
    max_publication_class: "content-shared",
    max_classification: "internal",
    policy_version: 1,
    created_at: "2026-08-17T00:00:00Z",
  };
}

function sessionWire() {
  return {
    id: "session-1",
    organization_id: "org-1",
    repository_id: "repo-1",
    daemon_id: "daemon-1",
    remote_session_key: "local-key",
    class: "metadata-shared",
    title: null,
    state: "running",
    started_at: "2026-08-17T00:00:00Z",
    last_activity_at: null,
    tombstoned_at: null,
    updated_at: "2026-08-17T00:00:00Z",
  };
}

function objectWire() {
  return {
    id: "object-1",
    organization_id: "org-1",
    repository_id: null,
    content_hash: "a".repeat(64),
    byte_length: 5,
    media_type: "text/plain",
    class: "metadata-shared",
    encryption: "none",
    state: "available",
    uploaded_by_daemon: null,
    created_at: "2026-08-17T00:00:00Z",
  };
}
