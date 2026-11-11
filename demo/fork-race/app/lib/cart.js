export const cart = [
  { sku: "mug", name: "Terminal Mug", price_cents: 1800, quantity: 2 },
  { sku: "tee", name: "Segfault Tee", price_cents: 2600, quantity: 1 },
  { sku: "stickers", name: "Sticker Pack", price_cents: 600, quantity: 3 },
];

export const shipping_cents = 500;

export function subtotalCents(items = cart) {
  return items.reduce((sum, item) => sum + item.price_cents * item.quantity, 0);
}
