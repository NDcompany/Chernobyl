/**
 * Bridges React Hook Form submissions to Zod schemas and maps Zod issues
 * back onto fields as RHF errors. (Kept dependency-free instead of pulling
 * in @hookform/resolvers and its optional peer tree.)
 */
import type { FieldValues, Path, UseFormReturn } from 'react-hook-form';
import type { z } from 'zod';

export function zodHandler<T extends FieldValues>(
  schema: z.ZodType<T>,
  form: UseFormReturn<T>,
  onValid: (data: T) => void,
) {
  const runner = form.handleSubmit((values) => {
    const result = schema.safeParse(values);
    form.clearErrors();
    if (result.success) {
      onValid(result.data);
      return;
    }
    for (const issue of result.error.issues) {
      const path = issue.path.join('.');
      if (path) {
        form.setError(path as Path<T>, { type: 'validation', message: issue.message });
      }
    }
  });
  // RHF's no-resolver handleSubmit refuses to run the success callback while
  // any custom errors exist, so clear the previous round before validating.
  return (e?: Parameters<typeof runner>[0]) => {
    form.clearErrors();
    return runner(e);
  };
}
