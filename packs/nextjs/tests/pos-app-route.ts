import { NextResponse } from "next/server";

export async function GET(request: Request, { params }: { params: { orderId: string } }) {
  return NextResponse.json({ id: params.orderId, url: request.url });
}

export const DELETE = async () => new Response(null, { status: 204 });
