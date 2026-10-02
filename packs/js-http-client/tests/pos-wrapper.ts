import { BASE, request } from "./client";

export function getPlan(id: string) {
  return request<Plan>(BASE, `/v1/plans/${id}`);
}

export function cancelPlan(id: string) {
  return request<Plan>(BASE, `/v1/plans/${id}/cancel`, { method: "POST", body: { reason: "x" } });
}

class PlansClient {
  async archive(id: string) {
    return this.post(`/v1/plans/${id}/archive`, {});
  }

  private async post(path: string, body: unknown) {
    return fetch(path, { method: "POST", body: JSON.stringify(body) });
  }
}
