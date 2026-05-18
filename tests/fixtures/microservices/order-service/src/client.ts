import { UserInfo, PaymentRequest, PaymentResult } from "./models";

const PAYMENT_SERVICE_URL = "http://payment-service:8080";

export class UserServiceClient {
  private baseUrl: string;
  private timeout: number;

  constructor(baseUrl: string, timeout: number = 5000) {
    this.baseUrl = baseUrl;
    this.timeout = timeout;
  }

  async getUser(userId: string): Promise<UserInfo | null> {
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), this.timeout);

      const response = await fetch(`${this.baseUrl}/api/users/${userId}`, {
        headers: { "Content-Type": "application/json" },
        signal: controller.signal,
      });

      clearTimeout(timer);

      if (!response.ok) {
        if (response.status === 404) return null;
        throw new Error(`User service returned ${response.status}`);
      }

      return (await response.json()) as UserInfo;
    } catch (err) {
      console.error(`Failed to fetch user ${userId}:`, err);
      return null;
    }
  }

  async validateUserExists(userId: string): Promise<boolean> {
    const user = await this.getUser(userId);
    return user !== null;
  }

  async initiatePayment(request: PaymentRequest): Promise<PaymentResult> {
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), this.timeout);

      const response = await fetch(`${PAYMENT_SERVICE_URL}/api/payments`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request),
        signal: controller.signal,
      });

      clearTimeout(timer);

      if (!response.ok) {
        return { success: false, error: `Payment service returned ${response.status}` };
      }

      const data = await response.json();
      return {
        success: true,
        paymentId: data.id,
      };
    } catch (err) {
      console.error("Payment initiation failed:", err);
      return { success: false, error: "Payment service unavailable" };
    }
  }

  async refundPayment(paymentId: string): Promise<boolean> {
    try {
      const response = await fetch(
        `${PAYMENT_SERVICE_URL}/api/payments/${paymentId}/refund`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
        }
      );
      return response.ok;
    } catch (err) {
      console.error(`Refund failed for payment ${paymentId}:`, err);
      return false;
    }
  }
}
