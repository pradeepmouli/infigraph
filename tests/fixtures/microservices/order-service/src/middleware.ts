import { Request, Response, NextFunction } from "express";

const AUTH_SERVICE_URL = "http://user-service:5000";

interface TokenClaims {
  sub: number;
  role: string;
  iat: number;
  exp: number;
}

declare global {
  namespace Express {
    interface Request {
      userId?: number;
      userRole?: string;
    }
  }
}

export async function authMiddleware(
  req: Request,
  res: Response,
  next: NextFunction
): Promise<void> {
  const authHeader = req.headers.authorization;

  if (!authHeader || !authHeader.startsWith("Bearer ")) {
    res.status(401).json({ error: "Missing authorization header" });
    return;
  }

  const token = authHeader.slice(7);

  try {
    const claims = decodeToken(token);
    if (!claims) {
      res.status(401).json({ error: "Invalid token" });
      return;
    }

    if (claims.exp < Date.now() / 1000) {
      res.status(401).json({ error: "Token expired" });
      return;
    }

    req.userId = claims.sub;
    req.userRole = claims.role;
    next();
  } catch (err) {
    res.status(401).json({ error: "Token validation failed" });
  }
}

export function requireRole(...roles: string[]) {
  return (req: Request, res: Response, next: NextFunction): void => {
    if (!req.userRole || !roles.includes(req.userRole)) {
      res.status(403).json({ error: "Insufficient permissions" });
      return;
    }
    next();
  };
}

function decodeToken(token: string): TokenClaims | null {
  const parts = token.split(".");
  if (parts.length !== 3) return null;

  try {
    const payload = JSON.parse(
      Buffer.from(parts[1], "base64url").toString("utf-8")
    );
    return payload as TokenClaims;
  } catch {
    return null;
  }
}

export function requestLogger(
  req: Request,
  _res: Response,
  next: NextFunction
): void {
  const start = Date.now();
  console.log(`[${new Date().toISOString()}] ${req.method} ${req.path}`);
  next();
}
