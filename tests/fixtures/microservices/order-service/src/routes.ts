import { Router, Request, Response } from "express";
import { Order, OrderStatus, CreateOrderRequest } from "./models";
import { UserServiceClient } from "./client";
import { authMiddleware } from "./middleware";

const router = Router();
const client = new UserServiceClient("http://user-service:5000");

let orders: Map<string, Order> = new Map();
let nextId = 1;

function generateOrderId(): string {
  const id = `ORD-${String(nextId).padStart(6, "0")}`;
  nextId++;
  return id;
}

router.get("/api/orders", authMiddleware, async (req: Request, res: Response) => {
  const userId = req.query.user_id as string | undefined;
  const status = req.query.status as OrderStatus | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const perPage = parseInt(req.query.per_page as string) || 20;

  let filtered = Array.from(orders.values());

  if (userId) {
    filtered = filtered.filter((o) => o.userId === userId);
  }
  if (status) {
    filtered = filtered.filter((o) => o.status === status);
  }

  const start = (page - 1) * perPage;
  const paged = filtered.slice(start, start + perPage);

  res.json({
    orders: paged,
    page,
    per_page: perPage,
    total: filtered.length,
  });
});

router.post("/api/orders", authMiddleware, async (req: Request, res: Response) => {
  const body: CreateOrderRequest = req.body;

  if (!body.userId || !body.items || body.items.length === 0) {
    res.status(400).json({ error: "userId and items are required" });
    return;
  }

  const user = await client.getUser(body.userId);
  if (!user) {
    res.status(404).json({ error: "User not found" });
    return;
  }

  const totalAmount = body.items.reduce(
    (sum, item) => sum + item.price * item.quantity,
    0
  );

  const order: Order = {
    id: generateOrderId(),
    userId: body.userId,
    items: body.items,
    totalAmount,
    status: "pending",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
  };

  orders.set(order.id, order);

  const paymentResult = await client.initiatePayment({
    orderId: order.id,
    amount: totalAmount,
    currency: body.currency || "USD",
  });

  if (paymentResult.success) {
    order.status = "confirmed";
    order.paymentId = paymentResult.paymentId;
  } else {
    order.status = "payment_failed";
  }
  order.updatedAt = new Date().toISOString();

  res.status(201).json(order);
});

router.get("/api/orders/:id", authMiddleware, async (req: Request, res: Response) => {
  const order = orders.get(req.params.id);
  if (!order) {
    res.status(404).json({ error: "Order not found" });
    return;
  }
  res.json(order);
});

router.delete("/api/orders/:id", authMiddleware, async (req: Request, res: Response) => {
  const order = orders.get(req.params.id);
  if (!order) {
    res.status(404).json({ error: "Order not found" });
    return;
  }
  if (order.status === "shipped" || order.status === "delivered") {
    res.status(409).json({ error: "Cannot cancel shipped/delivered order" });
    return;
  }
  order.status = "cancelled";
  order.updatedAt = new Date().toISOString();

  if (order.paymentId) {
    await client.refundPayment(order.paymentId);
  }

  res.json({ deleted: true, order });
});

router.post("/api/orders/cancel-all", authMiddleware, async (req: Request, res: Response) => {
  const { user_id } = req.body;
  let count = 0;
  for (const order of orders.values()) {
    if (order.userId === user_id && order.status === "pending") {
      order.status = "cancelled";
      order.updatedAt = new Date().toISOString();
      count++;
    }
  }
  res.json({ cancelled: count });
});

export default router;
