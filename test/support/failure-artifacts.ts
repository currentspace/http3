import {
  binding,
  type LifecycleTraceSnapshot,
  type ReactorTelemetrySnapshot,
} from '../../lib/event-loop.js';

export interface LifecycleFailureArtifacts {
  label: string;
  runtimeTelemetry: ReactorTelemetrySnapshot;
  lifecycleTrace: LifecycleTraceSnapshot;
}

export function beginLifecycleCapture(): void {
  binding.resetRuntimeTelemetry();
  binding.resetLifecycleTrace();
  binding.setLifecycleTraceEnabled(true);
}

export function endLifecycleCapture(): void {
  binding.setLifecycleTraceEnabled(false);
}

export function captureLifecycleFailureArtifacts(label: string): LifecycleFailureArtifacts {
  return {
    label,
    runtimeTelemetry: binding.runtimeTelemetry(),
    lifecycleTrace: binding.lifecycleTraceSnapshot(),
  };
}

export function formatLifecycleFailureArtifacts(artifacts: LifecycleFailureArtifacts): string {
  return JSON.stringify(artifacts, null, 2);
}

export function appendLifecycleArtifacts(error: unknown, label: string): void {
  if (!(error instanceof Error)) return;
  const artifacts = captureLifecycleFailureArtifacts(label);
  error.message = `${error.message}\nLifecycle artifacts:\n${formatLifecycleFailureArtifacts(artifacts)}`;
}
