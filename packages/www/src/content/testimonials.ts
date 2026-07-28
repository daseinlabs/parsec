// TODO(testimonials): PLACEHOLDER QUOTES — every quote, name, and company
// below is fictional. Replace with real, permissioned quotes before launch.
// The section renders whatever is in this array; swapping copy is the only
// edit needed.

export type Testimonial = {
  quote: string;
  name: string;
  role: string;
  company: string;
};

export const TESTIMONIALS: Testimonial[] = [
  {
    quote:
      "By turn thirty our agents were dragging the entire transcript into " +
      "every request. parsec was the first tool that trimmed it without " +
      "asking me to trust a summary I couldn't inspect.",
    name: "Mara Okonkwo",
    role: "Staff engineer",
    company: "Draywick Labs",
  },
  {
    quote:
      "The ledger is what convinced me. Every request shows the " +
      "counterfactual next to what was actually sent — I didn't have to " +
      "believe a dashboard, I could read the receipts.",
    name: "Anders Vieth",
    role: "Infrastructure lead",
    company: "Larkspur Systems",
  },
  {
    quote:
      "My Max plan used to hit the weekly wall by Thursday. Long agent " +
      "sessions just keep going now — same model, same credentials, nothing " +
      "routed through anyone else's cloud.",
    name: "Priya Raghunath",
    role: "Agent platform engineer",
    company: "Ninebark Automation",
  },
];
