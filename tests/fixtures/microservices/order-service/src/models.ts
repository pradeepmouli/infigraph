export type OrderStatus =
  | "pending"
  | "confirmed"
  | "payment_failed"
  | "shipped"
  | "delivered"
  | "cancelled";

export interface OrderItem {
  productId: string;
  name: string;
  quantity: number;
  price: number;
}

export interface Order {
  id: string;
  userId: string;
  items: OrderItem[];
  totalAmount: number;
  status: OrderStatus;
  paymentId?: string;
  shippingAddress?: Address;
  createdAt: string;
  updatedAt: string;
}

export interface Address {
  street: string;
  city: string;
  state: string;
  zip: string;
  country: string;
}

export interface CreateOrderRequest {
  userId: string;
  items: OrderItem[];
  currency?: string;
  shippingAddress?: Address;
}

export interface PaymentRequest {
  orderId: string;
  amount: number;
  currency: string;
}

export interface PaymentResult {
  success: boolean;
  paymentId?: string;
  error?: string;
}

export interface UserInfo {
  id: number;
  name: string;
  email: string;
  role: string;
}
