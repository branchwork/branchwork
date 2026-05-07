import { Component, type ReactNode, type ErrorInfo } from "react";
import { Button } from "./ui/Button.js";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("[ErrorBoundary]", error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex items-center justify-center h-screen bg-gray-950 text-gray-100">
          <div className="max-w-lg p-6 bg-gray-900 border border-red-800/50 rounded-lg">
            <h2 className="text-lg font-bold text-red-400 mb-2">Something went wrong</h2>
            <p className="text-sm text-gray-400 mb-4">{this.state.error.message}</p>
            <pre className="text-xs text-gray-600 bg-gray-800 p-3 rounded overflow-auto max-h-40 mb-4">
              {this.state.error.stack}
            </pre>
            <Button
              variant="primary"
              size="sm"
              onClick={() => {
                this.setState({ error: null });
                window.location.reload();
              }}
            >
              Reload
            </Button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
