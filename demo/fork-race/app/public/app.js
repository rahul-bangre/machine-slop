const money = (cents) => `$${(cents / 100).toFixed(2)}`;

export function render(quote) {
  document.getElementById("items").innerHTML = quote.items.map((item) => `
        <div class="item">
          <div class="thumb">${item.name[0]}</div>
          <div class="name">${item.name}<small>Qty ${item.quantity}</small></div>
          <div class="price">${money(item.price_cents * item.quantity)}</div>
        </div>`).join("");
  document.getElementById("subtotal").textContent = money(quote.subtotal_cents);
  document.getElementById("shipping").textContent = money(quote.shipping_cents);
  document.getElementById("total").textContent = money(quote.total_cents);
}

const response = await fetch("/api/cart");
render(await response.json());

new EventSource("/__reload").onmessage = (event) => {
  if (event.data === "reload") location.reload();
};
