const steps = [
  '1. Ensure local server is running with TLS on 443-compatible port.',
  '2. Open Safari and navigate to the staging HTTPS endpoint.',
  '3. Open Web Inspector > Network and confirm requests succeed.',
  '4. Validate fetch API route returns expected JSON payload.',
  '5. Validate EventSource/SSE stream connects and receives at least one event.',
  '6. Reload page and verify EventSource reconnect behavior.',
  '7. Capture screenshots of successful fetch + SSE state.',
  '8. Run curl protocol checks against the same endpoint:',
  '   - curl --http3-only -k -s -D /dev/stderr https://<host>/',
  '   - curl --http2 -k -s -D /dev/stderr https://<host>/',
  '9. Save Safari version, OS version, endpoint URL, and evidence artifacts.',
];

console.log('Safari release checklist:');
for (const step of steps) {
  console.log(step);
}

