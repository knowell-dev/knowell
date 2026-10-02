import axios from "axios";

const api = axios.create({ baseURL: "/api" });

export const listCoupons = () => api.get("/v1/coupons");

export async function redeem(code: string) {
  return axios.post(`/v1/coupons/${code}/redeem`, { code });
}

export async function purge() {
  return axios({ method: "delete", url: "/v1/legacy/items" });
}
