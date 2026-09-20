// Health check endpoint used by the container HEALTHCHECK and Homepage widget.
export function GET() {
  return new Response(JSON.stringify({ ok: true }), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}
