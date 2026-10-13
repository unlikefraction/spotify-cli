import { EmptyState } from "@/components/silicon-ui/empty-state/empty-state";
export default function NotFound() {
  return <main className="landing"><section className="hero"><h1 className="sr-only">Page not found</h1><EmptyState title="Not here." description="Page not found. Try the quick start or spotify docs." action={<a href="/docs/usage/">Read the quick start →</a>}/></section></main>;
}
