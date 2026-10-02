const API = process.env.API_BASE ?? "";

export async function loadInvoices(customerId: string) {
  const res = await fetch(`${API}/v1/customers/${customerId}/invoices`);
  return res.json();
}

export async function voidInvoice(id: string) {
  await fetch(`/v1/invoices/${encodeURIComponent(id)}/void`, { method: "POST" });
}

export async function ping() {
  return fetch("https://status.example.com/health");
}
